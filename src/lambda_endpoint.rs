//! `run --lambda-endpoint`: forwards dynamic requests to a real AWS_IAM Lambda Function URL,
//! signed with SigV4, while the runner keeps serving static files locally. Development and
//! testing only; production ingress is the edge's job (LAMBDA-PLAN.md Phase 7).

use anyhow::{Context, Result, bail};
use aws_credential_types::Credentials;
use aws_sigv4::{
    http_request::{PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings, sign},
    sign::v4,
};
use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use reqwest::{Client, Url};
use std::time::{Duration, SystemTime};
use tracing::info;

/// Lambda's synchronous request payload limit, from the error Lambda returned in Phase 5
/// ("Request must be smaller than 6291456 bytes"). The event adds overhead (and base64 for
/// binary bodies), so Lambda may still reject smaller bodies itself.
const LAMBDA_REQUEST_LIMIT: usize = 6_291_456;
const HOP_BY_HOP: [&str; 7] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
];
/// Request headers never forwarded: the signature owns `authorization` and `x-amz-*`
/// (an AWS_IAM Function URL consumes `authorization` anyway), and the runner sets
/// `x-forwarded-host` itself.
const REPLACED_REQUEST_HEADERS: [&str; 7] = [
    "host",
    "content-length",
    "authorization",
    "x-amz-date",
    "x-amz-security-token",
    "x-amz-content-sha256",
    "x-forwarded-host",
];

pub struct LambdaEndpoint {
    base: Url,
    region: String,
    credentials: Credentials,
    client: Client,
}

/// Parses `--lambda-endpoint`: an https origin such as `https://<id>.lambda-url.<region>.on.aws/`.
/// Plain http is accepted only for loopback hosts (tests and local proxies).
pub fn parse_endpoint(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|error| format!("invalid URL: {error}"))?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err("must be an https URL".into());
    }
    if url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("must be a bare origin, like https://<id>.lambda-url.<region>.on.aws/".into());
    }
    Ok(url)
}

pub fn parse_region(value: &str) -> Result<String, String> {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        Ok(value.to_owned())
    } else {
        Err("must be an AWS region such as us-east-2".into())
    }
}

impl LambdaEndpoint {
    /// Credentials come from the runner's own environment and are never passed to workers
    /// (none are started in this mode) or logged.
    pub fn from_env(base: Url, region: String) -> Result<Self> {
        Self::new(base, region, |name| std::env::var(name).ok())
    }

    fn new(base: Url, region: String, lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let value = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        let (Some(access_key), Some(secret_key)) =
            (value("AWS_ACCESS_KEY_ID"), value("AWS_SECRET_ACCESS_KEY"))
        else {
            bail!(
                "--lambda-endpoint needs AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY (and AWS_SESSION_TOKEN for temporary credentials) in the environment, for example from `aws configure export-credentials --format env`"
            );
        };
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build()
            .context("failed to create the Lambda endpoint HTTP client")?;
        Ok(Self {
            base,
            region,
            credentials: Credentials::new(
                access_key,
                secret_key,
                value("AWS_SESSION_TOKEN"),
                None,
                "meshscale-lambda-endpoint",
            ),
            client,
        })
    }

    pub fn origin(&self) -> String {
        self.base.origin().ascii_serialization()
    }

    pub async fn forward(&self, request: Request) -> Result<Response> {
        let (parts, body) = request.into_parts();
        let declared = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok());
        if declared.is_some_and(|length| length > LAMBDA_REQUEST_LIMIT) {
            return Ok(too_large());
        }
        let Ok(body) = axum::body::to_bytes(body, LAMBDA_REQUEST_LIMIT).await else {
            return Ok(too_large());
        };

        let mut url = self.base.clone();
        url.set_path(parts.uri.path());
        url.set_query(parts.uri.query());
        let mut headers = forwarded_headers(&parts.headers);
        if let Some(host) = parts.headers.get(header::HOST) {
            headers.insert("x-forwarded-host", host.clone());
        }
        for (name, value) in
            self.signing_headers(parts.method.as_str(), &url, &body, SystemTime::now())?
        {
            headers.insert(name, value);
        }

        info!(method = %parts.method, path = %parts.uri.path(), "forwarding to the Lambda endpoint");
        let response = self
            .client
            .request(parts.method.clone(), url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .context("Lambda endpoint request failed")?;

        let mut builder = Response::builder().status(response.status());
        for (name, value) in response.headers() {
            if !HOP_BY_HOP.contains(&name.as_str()) {
                builder = builder.header(name, value);
            }
        }
        builder
            .body(Body::from_stream(response.bytes_stream()))
            .context("failed to build the Lambda endpoint response")
    }

    /// SigV4 headers (`authorization`, `x-amz-date`, `x-amz-content-sha256`, and
    /// `x-amz-security-token` for temporary credentials) for service `lambda`. AWS_IAM Function
    /// URLs need the real payload hash in `x-amz-content-sha256` for requests with a body.
    fn signing_headers(
        &self,
        method: &str,
        url: &Url,
        body: &[u8],
        time: SystemTime,
    ) -> Result<Vec<(HeaderName, HeaderValue)>> {
        let identity = self.credentials.clone().into();
        let mut settings = SigningSettings::default();
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("lambda")
            .time(time)
            .settings(settings)
            .build()?
            .into();
        let signable = SignableRequest::new(
            method,
            url.as_str(),
            std::iter::empty(),
            SignableBody::Bytes(body),
        )?;
        let (instructions, _) = sign(signable, &params)?.into_parts();
        instructions
            .headers()
            .map(|(name, value)| {
                let mut value = HeaderValue::from_str(value)?;
                value.set_sensitive(name == "authorization" || name == "x-amz-security-token");
                Ok((HeaderName::from_bytes(name.as_bytes())?, value))
            })
            .collect()
    }
}

fn forwarded_headers(incoming: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in incoming {
        let name_str = name.as_str();
        if !HOP_BY_HOP.contains(&name_str) && !REPLACED_REQUEST_HEADERS.contains(&name_str) {
            headers.append(name, value.clone());
        }
    }
    headers
}

fn too_large() -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        format!("request body exceeds the {LAMBDA_REQUEST_LIMIT} byte Lambda request limit\n"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::to_bytes};
    use sha2::{Digest, Sha256};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Seen {
        requests: Vec<(String, String, HeaderMap, Vec<u8>)>,
    }

    async fn mock_lambda(
        state: axum::extract::State<Arc<Mutex<Seen>>>,
        request: Request,
    ) -> Response {
        let (parts, body) = request.into_parts();
        let body = to_bytes(body, usize::MAX).await.unwrap().to_vec();
        state.lock().unwrap().requests.push((
            parts.method.to_string(),
            parts.uri.to_string(),
            parts.headers,
            body,
        ));
        Response::builder()
            .status(201)
            .header("content-type", "text/plain")
            .header("set-cookie", "a=1; Path=/")
            .header("set-cookie", "b=2; Path=/")
            .body(Body::from("from lambda"))
            .unwrap()
    }

    async fn mock() -> (Url, Arc<Mutex<Seen>>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let state = Arc::new(Mutex::new(Seen::default()));
        let router = Router::new()
            .fallback(mock_lambda)
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (url, state, task)
    }

    fn endpoint(base: Url, session: Option<&str>) -> LambdaEndpoint {
        let session = session.map(str::to_owned);
        LambdaEndpoint::new(base, "us-east-2".into(), move |name| match name {
            "AWS_ACCESS_KEY_ID" => Some("test-access".into()),
            "AWS_SECRET_ACCESS_KEY" => Some("test-secret".into()),
            "AWS_SESSION_TOKEN" => session.clone(),
            _ => None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn forwards_signed_requests_and_streams_the_response_back() -> Result<()> {
        let (base, seen, task) = mock().await;
        let endpoint = endpoint(base.clone(), Some("test-session"));
        let body = b"\x00\x01binary\xff".to_vec();
        let request = Request::builder()
            .method("POST")
            .uri("/dynamic/a%20b?x=1&x=2")
            .header("host", "app.localhost:3000")
            .header("content-type", "application/octet-stream")
            .header("content-length", body.len())
            .header("cookie", "a=1")
            .header("cookie", "b=2")
            .header("authorization", "Basic client-supplied")
            .header("x-forwarded-host", "spoofed.example")
            .header("connection", "keep-alive")
            .body(Body::from(body.clone()))?;
        let response = endpoint.forward(request).await?;
        assert_eq!(response.status(), 201);
        let cookies = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(cookies, ["a=1; Path=/", "b=2; Path=/"]);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await?,
            "from lambda"
        );

        let seen = seen.lock().unwrap();
        let (method, uri, headers, received) = &seen.requests[0];
        assert_eq!(method, "POST");
        assert_eq!(uri, "/dynamic/a%20b?x=1&x=2", "path and query stay encoded");
        assert_eq!(received, &body);
        assert_eq!(headers["host"], base.authority());
        assert_eq!(headers["x-forwarded-host"], "app.localhost:3000");
        assert_eq!(headers.get_all("x-forwarded-host").iter().count(), 1);
        assert_eq!(headers.get_all("cookie").iter().count(), 2);
        assert!(headers.get("connection").is_none());
        assert_eq!(headers["x-amz-content-sha256"], hex(&Sha256::digest(&body)));
        assert_eq!(headers["x-amz-security-token"], "test-session");
        let authorization = headers["authorization"].to_str()?;
        assert_eq!(headers.get_all("authorization").iter().count(), 1);
        assert!(
            authorization.starts_with("AWS4-HMAC-SHA256 Credential=test-access/")
                && authorization.contains("/us-east-2/lambda/aws4_request"),
            "{authorization}"
        );
        assert!(!authorization.contains("test-secret"));
        task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn rejects_bodies_above_the_lambda_limit_without_forwarding() -> Result<()> {
        let (base, seen, task) = mock().await;
        let endpoint = endpoint(base, None);
        let big = vec![b'a'; LAMBDA_REQUEST_LIMIT + 1];
        for declared in [true, false] {
            let mut request = Request::builder()
                .method("POST")
                .uri("/route-handler")
                .header("host", "app.localhost");
            if declared {
                request = request.header("content-length", big.len());
            }
            let response = endpoint
                .forward(request.body(Body::from(big.clone()))?)
                .await?;
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        }
        assert!(seen.lock().unwrap().requests.is_empty());
        task.abort();
        Ok(())
    }

    #[test]
    fn signature_matches_an_independent_sigv4_computation() -> Result<()> {
        let endpoint = endpoint(
            Url::parse("https://abc.lambda-url.us-east-2.on.aws/")?,
            None,
        );
        let url = Url::parse("https://abc.lambda-url.us-east-2.on.aws/ssr?b=2&a=1")?;
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_547_200);
        let headers = endpoint.signing_headers("GET", &url, b"", time)?;
        let get = |wanted: &str| {
            headers
                .iter()
                .find(|(name, _)| name == wanted)
                .map(|(_, value)| value.to_str().unwrap().to_owned())
        };
        assert_eq!(get("x-amz-date").as_deref(), Some("20261009T120000Z"));
        // Recompute by hand: canonical request -> string to sign -> HMAC chain.
        let payload = hex(&Sha256::digest(b""));
        let canonical = format!(
            "GET\n/ssr\na=1&b=2\nhost:abc.lambda-url.us-east-2.on.aws\nx-amz-content-sha256:{payload}\nx-amz-date:20261009T120000Z\n\nhost;x-amz-content-sha256;x-amz-date\n{payload}"
        );
        let scope = "20261009/us-east-2/lambda/aws4_request";
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n20261009T120000Z\n{scope}\n{}",
            hex(&Sha256::digest(canonical.as_bytes()))
        );
        let mut key = hmac(b"AWS4test-secret", b"20261009");
        for part in ["us-east-2", "lambda", "aws4_request"] {
            key = hmac(&key, part.as_bytes());
        }
        let signature = hex(&hmac(&key, to_sign.as_bytes()));
        assert_eq!(
            get("authorization").as_deref(),
            Some(format!("AWS4-HMAC-SHA256 Credential=test-access/{scope}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}").as_str())
        );
        assert!(get("x-amz-security-token").is_none());
        Ok(())
    }

    #[test]
    fn validates_arguments_and_credentials() {
        assert!(parse_endpoint("https://abc.lambda-url.us-east-2.on.aws/").is_ok());
        assert!(parse_endpoint("https://abc.lambda-url.us-east-2.on.aws").is_ok());
        assert!(parse_endpoint("http://127.0.0.1:9000/").is_ok());
        for invalid in [
            "http://abc.lambda-url.us-east-2.on.aws/",
            "https://abc.lambda-url.us-east-2.on.aws/path",
            "https://abc.lambda-url.us-east-2.on.aws/?q=1",
            "https://user:pass@abc.lambda-url.us-east-2.on.aws/",
            "not a url",
        ] {
            assert!(parse_endpoint(invalid).is_err(), "{invalid}");
        }
        assert!(parse_region("us-east-2").is_ok());
        assert!(parse_region("US East").is_err());
        assert!(parse_region("").is_err());

        let base = Url::parse("https://abc.lambda-url.us-east-2.on.aws/").unwrap();
        let error = LambdaEndpoint::new(base.clone(), "us-east-2".into(), |name| {
            (name == "AWS_ACCESS_KEY_ID").then(|| "only-the-key".into())
        })
        .err()
        .unwrap();
        let message = format!("{error:#}");
        assert!(message.contains("AWS_SECRET_ACCESS_KEY"));
        assert!(!message.contains("only-the-key"));
        assert!(LambdaEndpoint::new(base, "us-east-2".into(), |_| Some("  ".into())).is_err());
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
        // HMAC-SHA256 from the definition, so the test does not depend on aws-sigv4 internals.
        let mut block = [0u8; 64];
        if key.len() > 64 {
            block[..32].copy_from_slice(&Sha256::digest(key));
        } else {
            block[..key.len()].copy_from_slice(key);
        }
        let inner = Sha256::new()
            .chain_update(block.map(|byte| byte ^ 0x36))
            .chain_update(data)
            .finalize();
        Sha256::new()
            .chain_update(block.map(|byte| byte ^ 0x5c))
            .chain_update(inner)
            .finalize()
            .to_vec()
    }
}
