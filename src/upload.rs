use anyhow::{Context, Result, bail, ensure};
use aws_credential_types::Credentials;
use aws_sigv4::{
    http_request::{
        PayloadChecksumKind, PercentEncodingMode, SignableBody, SignableRequest, SigningSettings,
        UriPathNormalizationMode, sign,
    },
    sign::v4,
};
use clap::Args;
use futures_util::{StreamExt, TryStreamExt, stream};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Client, StatusCode, Url, header};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tokio_util::io::ReaderStream;
use walkdir::WalkDir;

use crate::{artifact, manifest};

const MAX_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const RESERVATION: &str = "_upload.json";
const CONCURRENCY: usize = 4;
// S3 signs RFC 3986 paths, not WHATWG URL paths (which leave brackets and '+' raw).
const S3_PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');
const ENV_NAMES: [&str; 4] = [
    "MESHSCALE_R2_ACCOUNT_ID",
    "MESHSCALE_R2_ACCESS_KEY_ID",
    "MESHSCALE_R2_SECRET_ACCESS_KEY",
    "MESHSCALE_R2_BUCKET",
];

#[derive(Args, Debug, Clone)]
pub struct DestinationArgs {
    /// Organization identifier, used as the first R2 prefix segment.
    #[arg(long, requires = "project_id")]
    pub org_id: Option<String>,
    /// Project identifier, used as the second R2 prefix segment.
    #[arg(long, requires = "org_id")]
    pub project_id: Option<String>,
}

impl DestinationArgs {
    pub fn validate(&self) -> Result<(&str, &str)> {
        let org = self
            .org_id
            .as_deref()
            .context("--org-id is required for upload")?;
        let project = self
            .project_id
            .as_deref()
            .context("--project-id is required for upload")?;
        validate_id("org-id", org)?;
        validate_id("project-id", project)?;
        Ok((org, project))
    }

    pub fn validate_optional(&self) -> Result<()> {
        if self.org_id.is_some() || self.project_id.is_some() {
            self.validate()?;
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
pub struct UploadArgs {
    /// Path to a previously built .meshscale/output directory.
    pub output: PathBuf,
    #[command(flatten)]
    pub destination: DestinationArgs,
    /// Optional assertion; must match the build ID stored in manifest.json.
    #[arg(long)]
    pub build_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UploadResult {
    pub bucket: String,
    pub prefix: String,
    pub files: usize,
    pub bytes: u64,
    pub manifest_key: String,
}

pub struct R2Config {
    endpoint: Url,
    bucket: String,
    credentials: Credentials,
}

impl R2Config {
    pub fn load(env_file: Option<&Path>) -> Result<Self> {
        let defaults = load_dotenv(env_file)?;
        Self::from_values(|name| {
            resolve_setting(name, &defaults, |candidate| {
                match std::env::var(candidate) {
                    Ok(value) => Ok(Some(value)),
                    Err(std::env::VarError::NotPresent) => Ok(None),
                    Err(std::env::VarError::NotUnicode(_)) => {
                        bail!("{candidate} is not valid UTF-8")
                    }
                }
            })
        })
    }

    fn from_values(mut lookup: impl FnMut(&str) -> Result<Option<String>>) -> Result<Self> {
        let mut values = Vec::new();
        for name in ENV_NAMES {
            // Accept the mixed-case spelling from the original plan as a legacy alias.
            let alias = name.replacen("MESHSCALE", "MESHScale", 1);
            let value = match lookup(name)? {
                Some(value) => value,
                None => lookup(&alias)?.with_context(|| format!("missing R2 setting {name}"))?,
            };
            ensure!(
                !value.trim().is_empty(),
                "R2 setting {name} cannot be empty"
            );
            values.push(value);
        }
        let account = &values[0];
        ensure!(
            account.len() == 32 && account.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "MESHSCALE_R2_ACCOUNT_ID must be a 32-character Cloudflare account ID"
        );
        let bucket = values[3].clone();
        ensure!(
            (3..=63).contains(&bucket.len())
                && bucket
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && bucket.as_bytes()[0].is_ascii_alphanumeric()
                && bucket.as_bytes()[bucket.len() - 1].is_ascii_alphanumeric(),
            "MESHSCALE_R2_BUCKET must be a valid R2 bucket name"
        );
        Ok(Self {
            endpoint: Url::parse(&format!("https://{account}.r2.cloudflarestorage.com"))?,
            bucket,
            credentials: Credentials::new(
                values[1].clone(),
                values[2].clone(),
                None,
                None,
                "meshscale-env",
            ),
        })
    }
}

fn resolve_setting(
    name: &str,
    defaults: &BTreeMap<String, String>,
    mut environment: impl FnMut(&str) -> Result<Option<String>>,
) -> Result<Option<String>> {
    let alias = name.replacen("MESHSCALE", "MESHScale", 1);
    for candidate in [name, alias.as_str()] {
        if let Some(value) = environment(candidate)? {
            return Ok(Some(value));
        }
    }
    Ok(defaults.get(name).or_else(|| defaults.get(&alias)).cloned())
}

fn load_dotenv(path: Option<&Path>) -> Result<BTreeMap<String, String>> {
    let values = if let Some(path) = path {
        dotenvy::from_path_iter(path)
            .map_err(|_| anyhow::anyhow!("failed to open dotenv file {}", path.display()))?
    } else {
        match dotenvy::dotenv_iter() {
            Ok(values) => values,
            Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
            }
            Err(_) => bail!("failed to open .env"),
        }
    };
    let mut result = BTreeMap::new();
    for value in values {
        // dotenv parse errors can contain the source line, including credentials.
        let (key, value) = value.map_err(|_| anyhow::anyhow!("invalid dotenv file syntax"))?;
        result.entry(key).or_insert(value);
    }
    Ok(result)
}

pub fn remove_credentials(command: &mut std::process::Command) {
    for name in ENV_NAMES {
        command.env_remove(name);
        command.env_remove(name.replacen("MESHSCALE", "MESHScale", 1));
    }
}

pub fn validate_id(name: &str, id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
        "{name} must contain 1-128 ASCII letters, digits, underscores or hyphens"
    );
    Ok(())
}

pub fn upload(args: UploadArgs, env_file: Option<&Path>) -> Result<()> {
    let result = upload_output(
        &args.output,
        &args.destination,
        args.build_id.as_deref(),
        R2Config::load(env_file)?,
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "status": "success", "upload": result
        }))?
    );
    Ok(())
}

pub fn upload_output(
    output: &Path,
    destination: &DestinationArgs,
    build_id: Option<&str>,
    config: R2Config,
) -> Result<UploadResult> {
    destination.validate()?;
    tokio::runtime::Runtime::new()
        .context("failed to create uploader executor")?
        .block_on(async {
            tokio::select! {
                result = upload_async(output, destination, build_id, config) => result,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for upload cancellation")?;
                    bail!("upload interrupted; prefix may contain partial objects. Use a new build ID");
                }
            }
        })
}

struct UploadFile {
    relative: String,
    path: PathBuf,
    bytes: u64,
}

/// Validates the entire artifact tree, copies only `static/` into the snapshot and verifies it
/// against the manifest, so later changes cannot affect what is uploaded.
fn snapshot(
    output: &Path,
    destination: &Path,
    manifest: &manifest::Manifest,
) -> Result<Vec<UploadFile>> {
    let root = output.canonicalize()?;
    for entry in WalkDir::new(output).follow_links(false) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(output)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let name = relative
            .to_str()
            .context("artifact path must be UTF-8")?
            .replace('\\', "/");
        manifest::validate_relative(&name)?;
        ensure!(
            matches!(
                name.split('/').next(),
                Some("runtime" | "static" | "manifest.json")
            ),
            "unexpected artifact entry {name}"
        );
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink() && fs::read_link(entry.path()).is_err(),
            "artifact contains a link: {name}"
        );
        ensure!(
            entry.path().canonicalize()?.starts_with(&root),
            "artifact escapes output"
        );
        if name != "static" && !name.starts_with("static/") {
            continue;
        }
        if metadata.is_dir() {
            fs::create_dir_all(destination.join(relative))?;
        } else {
            ensure!(metadata.is_file(), "unsupported artifact entry {name}");
            fs::create_dir_all(
                destination
                    .join(relative)
                    .parent()
                    .context("snapshot path has no parent")?,
            )?;
            fs::copy(entry.path(), destination.join(relative))?;
        }
    }
    crate::static_output::verify_inventory(destination, manifest)?;
    manifest
        .r#static
        .objects
        .iter()
        .map(|(key, object)| {
            ensure!(
                object.bytes <= MAX_OBJECT_BYTES,
                "static object exceeds 5 GiB upload limit: {key}"
            );
            Ok(UploadFile {
                relative: format!("static/{key}"),
                path: destination.join("static").join(key),
                bytes: object.bytes,
            })
        })
        .collect()
}

async fn upload_async(
    output: &Path,
    destination: &DestinationArgs,
    asserted_build_id: Option<&str>,
    config: R2Config,
) -> Result<UploadResult> {
    let (org, project) = destination.validate()?;
    artifact::validate_output(output)?;
    let mut manifest = manifest::load(output)?;
    manifest.validate_v2()?;
    let staging = tempfile::TempDir::new().context("failed to create upload snapshot")?;
    let files = snapshot(output, staging.path(), &manifest)?;
    validate_id("build-id", &manifest.deployment.id)?;
    if let Some(id) = asserted_build_id {
        ensure!(
            id == manifest.deployment.id,
            "--build-id does not match the artifact manifest"
        );
    }
    for (name, stored, supplied) in [
        ("org-id", manifest.deployment.org_id.as_deref(), org),
        (
            "project-id",
            manifest.deployment.project_id.as_deref(),
            project,
        ),
    ] {
        if let Some(stored) = stored {
            ensure!(
                stored == supplied,
                "--{name} does not match the artifact manifest"
            );
        }
    }
    manifest.bind(org, project, Some(&config.bucket))?;
    manifest::write(staging.path(), &manifest)?;
    let prefix = format!("{org}/{project}/{}/", manifest.deployment.id);
    let manifest_file = UploadFile {
        relative: "manifest.json".into(),
        path: staging.path().join("manifest.json"),
        bytes: fs::metadata(staging.path().join("manifest.json"))?.len(),
    };
    let total_files = files.len() + 1;
    let total_bytes = files.iter().try_fold(manifest_file.bytes, |total, file| {
        total
            .checked_add(file.bytes)
            .context("artifact byte count overflow")
    })?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()
        .context("failed to create R2 HTTP client")?;
    let uploader = Uploader { client, config };
    let reservation = serde_json::to_vec(&serde_json::json!({
        "version": 2, "org_id": org, "project_id": project,
        "build_id": manifest.deployment.id, "files": total_files, "bytes": total_bytes
    }))?;
    let result = async {
        uploader
            .put_bytes(&format!("{prefix}{RESERVATION}"), reservation)
            .await
            .context("failed to reserve immutable deployment prefix")?;
        let mut completed_bytes = 0;
        let mut completed_files = 0;
        report_progress(0, total_bytes, 0, total_files);
        let uploads = stream::iter(files.iter().map(|file| {
            let uploader = &uploader;
            let prefix = &prefix;
            let objects = &manifest.r#static.objects;
            async move {
                uploader
                    .put_static(
                        &format!("{prefix}{}", file.relative),
                        file,
                        objects
                            .get(file.relative.trim_start_matches("static/"))
                            .context("unknown static upload object")?,
                    )
                    .await?;
                Ok::<_, anyhow::Error>(file.bytes)
            }
        }))
        .buffer_unordered(CONCURRENCY);
        tokio::pin!(uploads);
        while let Some(bytes) = uploads.try_next().await? {
            completed_bytes += bytes;
            completed_files += 1;
            report_progress(completed_bytes, total_bytes, completed_files, total_files);
        }
        // The manifest is published last, only after every static object is acknowledged.
        manifest::write(output, &manifest)?;
        uploader
            .put_file(&format!("{prefix}manifest.json"), &manifest_file)
            .await?;
        report_progress(total_bytes, total_bytes, total_files, total_files);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    result.with_context(|| format!(
        "upload to {prefix} failed; objects are never overwritten or automatically deleted. Use a new build ID after a partial/uncertain upload"
    ))?;
    Ok(UploadResult {
        bucket: uploader.config.bucket,
        prefix,
        files: total_files,
        bytes: total_bytes,
        manifest_key: format!("{}/manifest.json", manifest.bound_prefix()?),
    })
}

fn report_progress(bytes: u64, total: u64, files: usize, total_files: usize) {
    let percent = progress_percent(bytes, total, files, total_files);
    eprintln!("Upload {percent:.1}% ({files}/{total_files} files, {bytes}/{total} bytes)");
}

fn progress_percent(bytes: u64, total: u64, files: usize, total_files: usize) -> f64 {
    if files == total_files {
        100.0
    } else {
        (bytes as f64 / total.max(1) as f64 * 100.0).min(99.9)
    }
}

struct Uploader {
    client: Client,
    config: R2Config,
}

impl Uploader {
    fn request(
        &self,
        key: &str,
        bytes: u64,
        content_type: &str,
    ) -> Result<reqwest::RequestBuilder> {
        self.request_at(key, bytes, content_type, SystemTime::now())
    }

    fn request_at(
        &self,
        key: &str,
        bytes: u64,
        content_type: &str,
        time: SystemTime,
    ) -> Result<reqwest::RequestBuilder> {
        let mut url = self.config.endpoint.clone();
        let path = std::iter::once(self.config.bucket.as_str())
            .chain(key.split('/'))
            .map(|segment| utf8_percent_encode(segment, S3_PATH_SEGMENT).to_string())
            .collect::<Vec<_>>()
            .join("/");
        url.set_path(&format!("/{path}"));
        let identity = self.config.credentials.clone().into();
        let mut settings = SigningSettings::default();
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        settings.percent_encoding_mode = PercentEncodingMode::Single;
        settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region("auto")
            .name("s3")
            .time(time)
            .settings(settings)
            .build()?
            .into();
        let signable = SignableRequest::new(
            "PUT",
            url.as_str(),
            [("if-none-match", "*"), ("content-type", content_type)].into_iter(),
            SignableBody::UnsignedPayload,
        )?;
        let (instructions, _) = sign(signable, &params)?.into_parts();
        let mut request = self
            .client
            .put(url)
            .header(header::IF_NONE_MATCH, "*")
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CONTENT_LENGTH, bytes);
        for (name, value) in instructions.headers() {
            let mut header = header::HeaderValue::from_str(value)?;
            header.set_sensitive(name == "authorization" || name == "x-amz-security-token");
            request = request.header(name, header);
        }
        Ok(request)
    }

    async fn put_bytes(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        let request = self
            .request(key, bytes.len() as u64, "application/json")?
            .body(bytes);
        self.send(key, request).await
    }

    async fn put_file(&self, key: &str, file: &UploadFile) -> Result<()> {
        let source = tokio::fs::File::open(&file.path)
            .await
            .with_context(|| format!("failed to open snapshot file {}", file.relative))?;
        let content_type = mime_guess::from_path(&file.path)
            .first_or_octet_stream()
            .to_string();
        let body = reqwest::Body::wrap_stream(ReaderStream::new(source));
        self.send(
            key,
            self.request(key, file.bytes, &content_type)?.body(body),
        )
        .await
    }

    async fn put_static(
        &self,
        key: &str,
        file: &UploadFile,
        object: &crate::manifest::StaticObject,
    ) -> Result<()> {
        ensure!(
            file.relative.starts_with("static/"),
            "runtime upload is prohibited"
        );
        let source = tokio::fs::File::open(&file.path).await?;
        let body = reqwest::Body::wrap_stream(ReaderStream::new(source));
        let request = self
            .request(key, file.bytes, &object.content_type)?
            .header(header::CACHE_CONTROL, &object.cache_control)
            .body(body);
        self.send(key, request).await
    }

    async fn send(&self, key: &str, request: reqwest::RequestBuilder) -> Result<()> {
        let mut response = request
            .send()
            .await
            .with_context(|| format!("R2 PUT failed for {key}"))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let request_id = response
            .headers()
            .get("x-amz-request-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| safe_error_token(value))
            .map(str::to_owned);
        let mut body = Vec::new();
        while body.len() < 16 * 1024 {
            let Some(chunk) = response
                .chunk()
                .await
                .with_context(|| format!("failed to read R2 error for {key}: HTTP {status}"))?
            else {
                break;
            };
            let remaining = 16 * 1024 - body.len();
            body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        }
        let code = r2_error_code(&body);
        let diagnostic = format!(
            "{}{}",
            code.as_ref()
                .map_or(String::new(), |code| format!(", R2 code {code}")),
            request_id.map_or(String::new(), |id| format!(", request ID {id}"))
        );
        if matches!(
            status,
            StatusCode::PRECONDITION_FAILED | StatusCode::CONFLICT
        ) {
            bail!(
                "immutable R2 object/prefix already exists or conflicts: {key} (HTTP {status}{diagnostic})"
            );
        }
        let hint = match code.as_deref() {
            Some("SignatureDoesNotMatch") => "; check request signing and system clock",
            Some("AccessDenied") => "; check R2 token permissions and bucket scope",
            _ => "",
        };
        bail!("R2 PUT failed for {key}: HTTP {status}{diagnostic}{hint}");
    }
}

fn safe_error_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn r2_error_code(body: &[u8]) -> Option<String> {
    // Do not include R2's raw XML: signature errors can echo credentials/canonical requests.
    let body = std::str::from_utf8(body).ok()?;
    let code = body.split_once("<Code>")?.1.split_once("</Code>")?.0;
    safe_error_token(code).then(|| code.to_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::{Request, State},
        response::IntoResponse,
    };
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Store {
        objects: BTreeMap<String, Vec<u8>>,
        order: Vec<String>,
        fail_asset: bool,
        fail_manifest: bool,
        signature_failure: bool,
        active: usize,
        max_active: usize,
    }

    type MockState = Arc<Mutex<Store>>;

    struct MockServer {
        endpoint: Url,
        store: MockState,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn mock_put(
        State(state): State<MockState>,
        request: Request,
    ) -> axum::response::Response {
        assert_eq!(request.method(), "PUT");
        assert_eq!(request.headers()["if-none-match"], "*");
        assert_eq!(
            request.headers()["x-amz-content-sha256"],
            "UNSIGNED-PAYLOAD"
        );
        let authorization = request.headers()["authorization"].to_str().unwrap();
        assert!(authorization.starts_with("AWS4-HMAC-SHA256 Credential=test-access/"));
        assert!(authorization.contains("/auto/s3/aws4_request"));
        assert!(authorization.contains("if-none-match"));
        assert!(!authorization.contains("test-secret"));
        let path = request.uri().path().to_owned();
        let length: usize = request.headers()["content-length"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let content_type = request.headers()["content-type"]
            .to_str()
            .unwrap()
            .to_owned();
        let bytes = to_bytes(request.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(length, bytes.len());
        {
            let mut state = state.lock().unwrap();
            state.order.push(path.clone());
            if state.signature_failure {
                return (
                    StatusCode::FORBIDDEN,
                    [("x-amz-request-id", "request-123")],
                    "<Error><Code>SignatureDoesNotMatch</Code><Message>secret-response-content</Message><AWSAccessKeyId>test-access</AWSAccessKeyId></Error>",
                ).into_response();
            }
            if state.objects.contains_key(&path) {
                return StatusCode::PRECONDITION_FAILED.into_response();
            }
            if (state.fail_asset && path.ends_with("bad.txt"))
                || (state.fail_manifest && path.ends_with("manifest.json"))
            {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            state.active += 1;
            state.max_active = state.max_active.max(state.active);
            assert!(state.objects.insert(path.clone(), bytes.to_vec()).is_none());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        let mut state = state.lock().unwrap();
        state.active -= 1;
        if path.ends_with(".json") {
            assert_eq!(content_type, "application/json");
        }
        (StatusCode::OK, Body::empty()).into_response()
    }

    async fn mock_server() -> Result<MockServer> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = Url::parse(&format!("http://{}", listener.local_addr()?))?;
        let store = Arc::new(Mutex::new(Store::default()));
        let router = Router::new().fallback(mock_put).with_state(store.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Ok(MockServer {
            endpoint,
            store,
            task,
        })
    }

    pub(crate) async fn publish_real_fixture(output: &Path) -> Result<UploadResult> {
        let server = mock_server().await?;
        let metadata = manifest::load(output)?;
        let destination = DestinationArgs {
            org_id: metadata.deployment.org_id,
            project_id: metadata.deployment.project_id,
        };
        let result =
            upload_async(output, &destination, None, config(server.endpoint.clone())).await?;
        let store = server.store.lock().unwrap();
        ensure!(
            !store.objects.keys().any(|key| key.contains("/runtime/")),
            "runtime leaked to R2"
        );
        ensure!(
            store
                .order
                .last()
                .is_some_and(|key| key.ends_with("/manifest.json")),
            "manifest not published last"
        );
        Ok(result)
    }

    fn config(endpoint: Url) -> R2Config {
        R2Config {
            endpoint,
            bucket: "test-bucket".to_owned(),
            credentials: Credentials::new("test-access", "test-secret", None, None, "test"),
        }
    }

    fn destination() -> DestinationArgs {
        DestinationArgs {
            org_id: Some("org_123".to_owned()),
            project_id: Some("project_456".to_owned()),
        }
    }

    struct Fixture {
        _root: tempfile::TempDir,
        output: PathBuf,
    }
    impl Fixture {
        fn path(&self) -> &Path {
            &self.output
        }
    }

    fn fixture() -> Result<Fixture> {
        let root = tempfile::TempDir::new()?;
        let output = Fixture {
            output: root.path().join("output"),
            _root: root,
        };
        let runtime = output.path().join("runtime");
        fs::create_dir_all(runtime.join(".next"))?;
        fs::create_dir_all(runtime.join("node_modules").join("next"))?;
        fs::create_dir_all(output.path().join("static"))?;
        fs::write(
            runtime
                .join("node_modules")
                .join("next")
                .join("package.json"),
            "{}",
        )?;
        fs::write(runtime.join("function-entry.cjs"), "// test")?;
        fs::write(
            runtime.join(".next").join("required-server-files.json"),
            r#"{"config":{}}"#,
        )?;
        fs::write(
            output.path().join("static").join("a +#%.txt"),
            "encoded asset",
        )?;
        fs::write(output.path().join("static").join("empty.txt"), "")?;
        fs::write(output.path().join("static").join("bad.txt"), "asset")?;
        fs::write(output.path().join("manifest.json"), serde_json::json!({
                    "version": 1, "framework": "nextjs",
                    "runtime": {"type":"node", "command":"node",
                        "entrypoint":"runtime/function-entry.cjs", "working_directory":"runtime", "args":[]},
                    "static": {"directory":"static"},
                    "deployment": {"id":"build_789", "repository":"test/repo", "commit":"test", "branch":"main",
                        "org_id":"org_123", "project_id":"project_456"}
                }).to_string())?;
        let mut metadata = manifest::load(output.path())?;
        metadata.version = 2;
        metadata.deployment.commit = "1234567890abcdef1234567890abcdef12345678".into();
        metadata.deployment.next_build_id = Some("next-build".into());
        metadata.deployment.next_version = Some("16.3.4".into());
        metadata.platform = Some(manifest::Platform {
            os: "windows".into(),
            arch: "x86_64".into(),
            node_version: "v25.8.2".into(),
            node_abi: "141".into(),
        });
        metadata.server = Some(manifest::Server {
            target_id: metadata.target_id(),
        });
        metadata.routing = Some(crate::routing::Routing::new());
        for file in ["a +#%.txt", "empty.txt", "bad.txt"] {
            let object = crate::static_output::object(
                &output.path().join("static").join(file),
                "public_asset",
            )?;
            metadata.r#static.objects.insert(file.into(), object);
        }
        fs::create_dir_all(runtime.join(".next").join("static"))?;
        fs::create_dir_all(runtime.join("empty"))?;
        fs::write(
            runtime.join(".next").join("static").join("keep.js"),
            "fallback",
        )?;
        manifest::write(output.path(), &metadata)?;
        Ok(output)
    }

    #[tokio::test]
    async fn uploads_exact_keys_and_bytes_and_publishes_manifest_last() -> Result<()> {
        let server = mock_server().await?;
        let output = fixture()?;
        let result = upload_async(
            output.path(),
            &destination(),
            Some("build_789"),
            config(server.endpoint.clone()),
        )
        .await?;
        assert_eq!(result.prefix, "org_123/project_456/build_789/");
        assert_eq!(result.files, 4);
        let store = server.store.lock().unwrap();
        let root = "/test-bucket/org_123/project_456/build_789/";
        assert_eq!(
            store.order.first().unwrap(),
            &format!("{root}{RESERVATION}")
        );
        assert_eq!(store.order.last().unwrap(), &format!("{root}manifest.json"));
        assert_eq!(store.objects.len(), result.files + 1);
        assert_eq!(
            store.objects[&format!("{root}static/a%20%2B%23%25.txt")],
            b"encoded asset"
        );
        assert_eq!(store.objects[&format!("{root}static/empty.txt")], b"");
        let remote_manifest = &store.objects[&format!("{root}manifest.json")];
        assert_eq!(
            *remote_manifest,
            fs::read(output.path().join("manifest.json"))?
        );
        assert!(store.max_active <= CONCURRENCY);
        assert!(store.max_active > 1);
        let bytes: u64 = store
            .objects
            .iter()
            .filter(|(key, _)| !key.ends_with(RESERVATION))
            .map(|(_, bytes)| bytes.len() as u64)
            .sum();
        assert_eq!(bytes, result.bytes);
        assert!(
            !store
                .objects
                .keys()
                .any(|key| key.contains("/runtime/") || key.contains(".zip"))
        );
        assert!(!String::from_utf8_lossy(remote_manifest).contains("\"archive\""));
        // Uploading must not modify the artifact's runtime or create a server archive.
        assert!(output.path().join("runtime/.next/static/keep.js").is_file());
        assert!(fs::read_dir(output.path().parent().unwrap())?.all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".zip")
        }));
        Ok(())
    }

    #[tokio::test]
    async fn uploads_nextjs_bracket_paths_without_changing_object_keys() -> Result<()> {
        let server = mock_server().await?;
        let uploader = Uploader {
            client: Client::new(),
            config: config(server.endpoint.clone()),
        };
        let key =
            "org/project/build/runtime/.next/build/chunks/[root-of-the-server]__1kki86f._.js.map";
        uploader.put_bytes(key, b"source map".to_vec()).await?;
        let store = server.store.lock().unwrap();
        let path = "/test-bucket/org/project/build/runtime/.next/build/chunks/%5Broot-of-the-server%5D__1kki86f._.js.map";
        assert_eq!(store.objects[path], b"source map");
        assert_eq!(
            percent_encoding::percent_decode_str(path).decode_utf8()?,
            format!("/test-bucket/{key}")
        );
        Ok(())
    }

    #[test]
    fn signs_fully_encoded_s3_path_and_sends_the_identical_uri() -> Result<()> {
        let uploader = Uploader {
            client: Client::new(),
            config: config(Url::parse("https://example.r2.cloudflarestorage.com")?),
        };
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let key = "org/project/build/[root]/a +#%()@!$&'=,;é~.js";
        let mut old_url = uploader.config.endpoint.clone();
        old_url
            .path_segments_mut()
            .unwrap()
            .clear()
            .push("test-bucket")
            .extend(key.split('/'));
        assert!(old_url.path().contains("[root]"));
        assert!(old_url.path().contains('+'));
        let expected_uri = "https://example.r2.cloudflarestorage.com/test-bucket/org/project/build/%5Broot%5D/a%20%2B%23%25%28%29%40%21%24%26%27%3D%2C%3B%C3%A9~.js";
        let request = uploader
            .request_at(key, 0, "application/json", time)?
            .build()?;
        assert_eq!(request.url().as_str(), expected_uri);
        let identity = uploader.config.credentials.clone().into();
        let mut settings = SigningSettings::default();
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        settings.percent_encoding_mode = PercentEncodingMode::Single;
        settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region("auto")
            .name("s3")
            .time(time)
            .settings(settings)
            .build()?
            .into();
        let signable = SignableRequest::new(
            "PUT",
            expected_uri,
            [("if-none-match", "*"), ("content-type", "application/json")].into_iter(),
            SignableBody::UnsignedPayload,
        )?;
        let (instructions, _) = sign(signable, &params)?.into_parts();
        for (name, value) in instructions.headers() {
            assert_eq!(request.headers()[name], value, "{name}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn reports_r2_error_code_without_echoing_sensitive_response_xml() -> Result<()> {
        let server = mock_server().await?;
        server.store.lock().unwrap().signature_failure = true;
        let uploader = Uploader {
            client: Client::new(),
            config: config(server.endpoint.clone()),
        };
        let error = uploader
            .put_bytes("org/project/build/file", Vec::new())
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("HTTP 403"));
        assert!(message.contains("SignatureDoesNotMatch"));
        assert!(message.contains("request-123"));
        for sensitive in ["secret-response-content", "test-access", "<Error>"] {
            assert!(!message.contains(sensitive), "{message}");
        }
        assert_eq!(
            r2_error_code(b"<Code>AccessDenied</Code>").as_deref(),
            Some("AccessDenied")
        );
        assert!(r2_error_code(b"<Code>secret\nvalue</Code>").is_none());
        assert!(r2_error_code(b"not xml").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn existing_deployment_cannot_be_overwritten() -> Result<()> {
        let server = mock_server().await?;
        let output = fixture()?;
        upload_async(
            output.path(),
            &destination(),
            None,
            config(server.endpoint.clone()),
        )
        .await?;
        let before = server.store.lock().unwrap().objects.clone();
        let error = upload_async(
            output.path(),
            &destination(),
            None,
            config(server.endpoint.clone()),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("already exists"));
        assert_eq!(server.store.lock().unwrap().objects, before);
        Ok(())
    }

    #[tokio::test]
    async fn failures_never_publish_a_complete_deployment() -> Result<()> {
        for manifest_failure in [false, true] {
            let server = mock_server().await?;
            {
                let mut store = server.store.lock().unwrap();
                store.fail_asset = !manifest_failure;
                store.fail_manifest = manifest_failure;
            }

            let output = fixture()?;
            let error = upload_async(
                output.path(),
                &destination(),
                None,
                config(server.endpoint.clone()),
            )
            .await
            .unwrap_err();
            assert!(format!("{error:#}").contains("HTTP 500"));
            assert!(format!("{error:#}").contains("new build ID"));
            assert!(
                !server
                    .store
                    .lock()
                    .unwrap()
                    .objects
                    .keys()
                    .any(|key| key.ends_with("manifest.json"))
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn validates_before_any_network_write() -> Result<()> {
        let server = mock_server().await?;
        let output = fixture()?;
        for (dest, id) in [
            (
                DestinationArgs {
                    org_id: Some("../escape".to_owned()),
                    project_id: Some("project".to_owned()),
                },
                None,
            ),
            (
                DestinationArgs {
                    org_id: Some("wrong_org".to_owned()),
                    project_id: Some("project_456".to_owned()),
                },
                None,
            ),
            (destination(), Some("wrong_build")),
        ] {
            assert!(
                upload_async(output.path(), &dest, id, config(server.endpoint.clone()))
                    .await
                    .is_err()
            );
        }
        fs::write(
            output.path().join("static").join(".env"),
            "SECRET=must-not-upload",
        )?;
        let error = upload_async(
            output.path(),
            &destination(),
            None,
            config(server.endpoint.clone()),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains(".env"));
        assert!(server.store.lock().unwrap().objects.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn legacy_artifacts_fail_before_remote_reservation() -> Result<()> {
        let server = mock_server().await?;
        let output = fixture()?;
        let metadata = manifest::load(output.path())?;
        let mut legacy = metadata;
        legacy.version = 1;
        manifest::write(output.path(), &legacy)?;
        let error = upload_async(
            output.path(),
            &self::destination(),
            None,
            config(server.endpoint.clone()),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("rebuild"));
        assert!(server.store.lock().unwrap().objects.is_empty());
        Ok(())
    }

    #[test]
    fn loads_dotenv_without_mutating_environment_and_redacts_parse_errors() -> Result<()> {
        let directory = tempfile::TempDir::new()?;
        let path = directory.path().join(".env");
        fs::write(
            &path,
            "MESHSCALE_R2_ACCESS_KEY_ID='file-access'\nMESHSCALE_R2_SECRET_ACCESS_KEY=\"file-secret\"\n",
        )?;
        let before = std::env::var("MESHSCALE_R2_ACCESS_KEY_ID");
        let values = load_dotenv(Some(&path))?;
        assert_eq!(values["MESHSCALE_R2_ACCESS_KEY_ID"], "file-access");
        assert_eq!(std::env::var("MESHSCALE_R2_ACCESS_KEY_ID"), before);
        fs::write(&path, "SECRET='do-not-echo")?;
        let error = load_dotenv(Some(&path)).unwrap_err();
        assert!(!format!("{error:#}").contains("do-not-echo"));
        assert!(load_dotenv(Some(&directory.path().join("missing"))).is_err());
        Ok(())
    }

    #[test]
    fn terminal_settings_override_dotenv_including_legacy_aliases() -> Result<()> {
        let name = ENV_NAMES[1];
        let defaults = BTreeMap::from([(name.to_owned(), "file-value".to_owned())]);
        let lookup = |environment: BTreeMap<&str, &str>| {
            resolve_setting(name, &defaults, |key| {
                Ok(environment.get(key).map(|value| value.to_string()))
            })
        };
        assert_eq!(lookup(BTreeMap::new())?.as_deref(), Some("file-value"));
        assert_eq!(
            lookup(BTreeMap::from([(name, "terminal")]))?.as_deref(),
            Some("terminal")
        );
        assert_eq!(
            lookup(BTreeMap::from([(
                "MESHScale_R2_ACCESS_KEY_ID",
                "legacy-terminal"
            )]))?
            .as_deref(),
            Some("legacy-terminal")
        );
        assert_eq!(lookup(BTreeMap::from([(name, "")]))?.as_deref(), Some(""));
        assert_eq!(
            lookup(BTreeMap::from([
                (name, "canonical"),
                ("MESHScale_R2_ACCESS_KEY_ID", "legacy")
            ]))?
            .as_deref(),
            Some("canonical")
        );
        Ok(())
    }

    #[test]
    fn progress_reaches_100_only_after_manifest_is_acknowledged() {
        assert_eq!(progress_percent(0, 100, 0, 3), 0.0);
        assert_eq!(progress_percent(50, 100, 1, 3), 50.0);
        assert_eq!(progress_percent(100, 100, 2, 3), 99.9);
        assert_eq!(progress_percent(100, 100, 3, 3), 100.0);
        assert_eq!(progress_percent(0, 0, 1, 2), 0.0);
    }

    #[tokio::test]
    async fn concurrent_uploaders_cannot_share_a_deployment_prefix() -> Result<()> {
        let server = mock_server().await?;
        let output = fixture()?;
        let dest = destination();
        let (first, second) = tokio::join!(
            upload_async(output.path(), &dest, None, config(server.endpoint.clone())),
            upload_async(output.path(), &dest, None, config(server.endpoint.clone())),
        );
        assert_ne!(first.is_ok(), second.is_ok());
        assert_eq!(server.store.lock().unwrap().objects.len(), 5);
        Ok(())
    }

    #[test]
    fn validates_configuration_and_ids() -> Result<()> {
        let values = BTreeMap::from([
            (ENV_NAMES[0], "0123456789abcdef0123456789abcdef"),
            (ENV_NAMES[1], "access"),
            (ENV_NAMES[2], "secret"),
            (ENV_NAMES[3], "bucket"),
        ]);
        let config =
            R2Config::from_values(|name| Ok(values.get(name).map(|value| value.to_string())))?;
        assert_eq!(
            config.endpoint.as_str(),
            "https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com/"
        );
        assert!(R2Config::from_values(|_| Ok(None)).is_err());
        assert!(R2Config::from_values(|_| Ok(Some(String::new()))).is_err());
        for id in [
            "",
            ".",
            "..",
            "org/project",
            "org\\project",
            "org:project",
            "org space",
            "é",
        ] {
            assert!(validate_id("test", id).is_err());
        }
        assert!(validate_id("test", &"a".repeat(129)).is_err());
        validate_id("test", "org_Project-123")?;
        let mut command = std::process::Command::new("node");
        command.env(ENV_NAMES[2], "secret");
        remove_credentials(&mut command);
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == ENV_NAMES[2] && value.is_none())
        );
        Ok(())
    }
}
