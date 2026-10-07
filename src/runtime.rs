use anyhow::{Context, Result, bail, ensure};
use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use clap::Args;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use percent_encoding::percent_decode_str;
use std::{
    future::{Future, IntoFuture},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    process::Child,
    sync::oneshot,
    time::{sleep, timeout},
};
use tower::ServiceExt;
use tower_http::services::ServeFile;
use tracing::{error, info};

use crate::{artifact, manifest};

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Path to a previously built .meshscale/output directory.
    pub output: PathBuf,
    /// Public HTTP port (loopback only).
    #[arg(long, default_value_t = 3000, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: u16,
    /// Internal Node HTTP port (loopback only).
    #[arg(long, default_value_t = 3100, value_parser = clap::value_parser!(u16).range(1..))]
    pub runtime_port: u16,
    #[arg(long, default_value_t = 256)]
    pub static_cache_mib: u64,
    #[arg(long, default_value_t = 16)]
    pub static_cache_max_file_mib: u64,
}

#[derive(Clone)]
struct AppState {
    assets: PathBuf,
    backend: SocketAddr,
    client: Client<HttpConnector, Body>,
    manifest: Option<Arc<manifest::Manifest>>,
    cache: Arc<crate::cache::StaticCache>,
}

pub fn run(args: RunArgs) -> Result<()> {
    tokio::runtime::Runtime::new()
        .context("failed to create local runner executor")?
        .block_on(run_with_shutdown(args, async {
            tokio::signal::ctrl_c()
                .await
                .context("failed to listen for Ctrl+C")
        }))
}

async fn run_with_shutdown(
    args: RunArgs,
    shutdown: impl Future<Output = Result<()>>,
) -> Result<()> {
    ensure!(
        args.port != args.runtime_port,
        "public and runtime ports must be different"
    );
    let cache_bytes = args
        .static_cache_mib
        .checked_mul(1024 * 1024)
        .context("cache budget overflow")?;
    let max_file = args
        .static_cache_max_file_mib
        .checked_mul(1024 * 1024)
        .context("cache per-file budget overflow")?;
    let cache = crate::cache::StaticCache::new(cache_bytes, max_file)?;
    artifact::validate_server_output(&args.output)?;
    let manifest = manifest::load_server(&args.output)?;
    if let Some(platform) = &manifest.platform {
        ensure!(
            platform.os == std::env::consts::OS && platform.arch == std::env::consts::ARCH,
            "server bundle platform mismatch: built for {}/{}",
            platform.os,
            platform.arch
        );
        let mut command = tokio::process::Command::new("node");
        crate::upload::remove_credentials(command.as_std_mut());
        let output = command
            .args(["-e", "console.log(process.versions.modules)"])
            .output()
            .await
            .context("failed to inspect local Node ABI")?;
        ensure!(output.status.success(), "failed to inspect local Node ABI");
        ensure!(
            String::from_utf8_lossy(&output.stdout).trim() == platform.node_abi,
            "Node ABI mismatch; use the Node version recorded in the manifest"
        );
    }
    let has_assets = args.output.join("static").is_dir();
    if manifest.version == 2 && has_assets {
        crate::static_output::verify_inventory(&args.output, &manifest)?;
    }
    let assets = if has_assets {
        manifest::resolve_path(&args.output, &manifest.r#static.directory)?
    } else {
        args.output.join("static")
    };
    let cwd = manifest::resolve_path(&args.output, &manifest.runtime.working_directory)?;
    let entrypoint = manifest::resolve_path(&args.output, &manifest.runtime.entrypoint)?;
    // Node's CLI does not reliably accept Windows verbatim paths returned by canonicalize.
    let entrypoint = entrypoint
        .strip_prefix(&cwd)
        .context("runtime entrypoint is outside its working directory")?;
    let listener = TcpListener::bind(loopback(args.port))
        .await
        .with_context(|| format!("failed to bind public port {}", args.port))?;
    let backend = loopback(args.runtime_port);
    let probe = TcpListener::bind(backend).await.with_context(|| {
        format!(
            "internal runtime port {} is already in use",
            args.runtime_port
        )
    })?;
    drop(probe);

    let mut command = tokio::process::Command::new(&manifest.runtime.command);
    crate::upload::remove_credentials(command.as_std_mut());
    command.arg(entrypoint).args(&manifest.runtime.args);
    if manifest.runtime.args == ["start"] {
        command.args([
            "--hostname",
            "127.0.0.1",
            "--port",
            &args.runtime_port.to_string(),
        ]);
    }
    let mut child = command
        .current_dir(cwd)
        .env("NODE_ENV", "production")
        .env("PORT", args.runtime_port.to_string())
        .env("HOSTNAME", "127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start Node runtime (is node on PATH?)")?;

    tokio::pin!(shutdown);
    let outcome = async {
        tokio::select! {
            result = wait_for_runtime(&mut child, backend) => result?,
            result = &mut shutdown => return result,
        }
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(Duration::from_secs(5)));
        let state = AppState {
            assets,
            backend,
            client: Client::builder(TokioExecutor::new()).build(connector),
            manifest: (manifest.version == 2).then(|| Arc::new(manifest)),
            cache,
        };
        let router = Router::new().fallback(route).with_state(state);
        let (stop, stopped) = oneshot::channel::<()>();
        let server = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async { let _ = stopped.await; })
            .into_future();
        tokio::pin!(server);
        info!(url = %format!("http://localhost:{}", args.port), runtime = %backend,
            static_cache_bytes = cache_bytes, static_cache_max_file_bytes = max_file, "local runner ready");
        tokio::select! {
            result = &mut server => result.context("local HTTP server failed"),
            status = child.wait() => {
                bail!("Node runtime exited unexpectedly: {}", status.context("failed to wait for Node runtime")?);
            }
            result = &mut shutdown => {
                result?;
                let _ = stop.send(());
                match timeout(Duration::from_secs(5), &mut server).await {
                    Ok(result) => result.context("local HTTP server failed during shutdown"),
                    Err(_) => {
                        info!("closing active HTTP connections after shutdown timeout");
                        Ok(())
                    }
                }
            }
        }
    }.await;

    let cleanup = stop_runtime(&mut child).await;
    if let Err(error) = &cleanup {
        error!(error = %format!("{error:#}"), "failed to stop Node runtime");
    }
    outcome?;
    cleanup
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

async fn stop_runtime(child: &mut Child) -> Result<()> {
    if child
        .try_wait()
        .context("failed to inspect Node runtime")?
        .is_none()
    {
        child.kill().await.context("failed to stop Node runtime")?;
    }
    child.wait().await.context("failed to reap Node runtime")?;
    Ok(())
}

async fn wait_for_runtime(child: &mut Child, address: SocketAddr) -> Result<()> {
    timeout(Duration::from_secs(30), async {
        loop {
            if let Some(status) = child.try_wait().context("failed to inspect Node runtime")? {
                bail!("Node runtime exited before becoming ready: {status}");
            }
            if TcpStream::connect(address).await.is_ok() {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("Node runtime did not become ready within 30 seconds")?
}

fn asset_relative_path(path: &str) -> Result<PathBuf> {
    // Decode before checking components so encoded separators cannot bypass traversal checks.
    let decoded = percent_decode_str(path)
        .decode_utf8()
        .context("request path is not valid UTF-8")?;
    ensure!(decoded.starts_with('/'), "request path must start with /");
    let mut relative = PathBuf::new();
    for segment in decoded.split('/').skip(1) {
        ensure!(
            segment != "." && segment != ".." && !segment.contains(['\\', ':', '\0']),
            "invalid request path"
        );
        if !segment.is_empty() {
            relative.push(segment);
        }
    }
    Ok(relative)
}

async fn route(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
) -> Response {
    let relative = match asset_relative_path(request.uri().path()) {
        Ok(path) => path,
        Err(error) => {
            error!(error = %error, "rejected invalid request path");
            return (StatusCode::BAD_REQUEST, "Invalid request path").into_response();
        }
    };
    if request.headers().contains_key(header::UPGRADE) {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "HTTP upgrades are not supported by the local runner",
        )
            .into_response();
    }

    let selected = if let Some(manifest) = &state.manifest {
        match manifest
            .routing
            .as_ref()
            .context("missing routing contract")
            .and_then(|routing| routing.select(request.method(), request.uri(), request.headers()))
        {
            Ok(object) => object.map(str::to_owned),
            Err(error) => {
                error!(error = %error, "invalid routed request");
                return StatusCode::BAD_REQUEST.into_response();
            }
        }
    } else if matches!(*request.method(), Method::GET | Method::HEAD) {
        Some(relative.to_string_lossy().replace('\\', "/"))
    } else {
        None
    };
    if let Some(selected) = selected {
        let object = state
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.r#static.objects.get(&selected));
        if let Some(object) = object
            && let Some(bytes) = state.cache.cached(&selected, object).await
        {
            match crate::cache::response(bytes, request.method(), request.headers(), object) {
                Ok(Some(response)) => return response,
                Ok(None) => {}
                Err(error) => {
                    error!(error = %error, "cached static response failed");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
        }
        let candidate = state.assets.join(&selected);
        match tokio::fs::metadata(&candidate).await {
            Ok(metadata) if metadata.is_file() => match tokio::fs::canonicalize(&candidate).await {
                Ok(path) if path.starts_with(&state.assets) => {
                    if let Some(object) = object {
                        if let Some(status) =
                            crate::cache::conditional_status(request.headers(), object)
                        {
                            let mut response = status.into_response();
                            if let Err(error) = crate::cache::apply_metadata(&mut response, object)
                            {
                                error!(error = %error, "invalid static metadata");
                                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                            }
                            return response;
                        }
                        match state
                            .cache
                            .get(&selected, &path, object)
                            .await
                            .and_then(|bytes| {
                                bytes
                                    .map(|bytes| {
                                        crate::cache::response(
                                            bytes,
                                            request.method(),
                                            request.headers(),
                                            object,
                                        )
                                    })
                                    .transpose()
                                    .map(Option::flatten)
                            }) {
                            Ok(Some(response)) => return response,
                            Ok(None) => {}
                            Err(error) => {
                                error!(error = %format!("{error:#}"), "static cache fill failed");
                                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                            }
                        }
                    }
                    if let Some(object) = object {
                        // Conditions were evaluated against manifest validators above, not
                        // mutable filesystem timestamps. Keep the streaming path identical.
                        for name in [
                            header::IF_MATCH,
                            header::IF_NONE_MATCH,
                            header::IF_MODIFIED_SINCE,
                            header::IF_UNMODIFIED_SINCE,
                        ] {
                            request.headers_mut().remove(name);
                        }
                        if let Some(value) = request.headers_mut().remove(header::IF_RANGE) {
                            let allowed = value.to_str().ok().is_some_and(|value| {
                                value == object.etag()
                                    || httpdate::parse_http_date(value)
                                        .ok()
                                        .zip(httpdate::parse_http_date(&object.last_modified).ok())
                                        .is_some_and(|(date, modified)| date >= modified)
                            });
                            if !allowed {
                                request.headers_mut().remove(header::RANGE);
                            }
                        }
                    }
                    match ServeFile::new(path).oneshot(request).await {
                        Ok(response) => {
                            let mut response = response.map(Body::new);
                            if let Some(object) = object
                                && let Err(error) =
                                    crate::cache::apply_metadata(&mut response, object)
                            {
                                error!(error = %error, "invalid static metadata");
                                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                            }
                            return response;
                        }
                        Err(error) => match error {},
                    }
                }
                Ok(_) => {
                    error!(path = %candidate.display(), "static asset escapes output directory");
                    return StatusCode::FORBIDDEN.into_response();
                }
                Err(error) => {
                    error!(error = %error, "failed to resolve static asset");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            },
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => {
                error!(error = %error, path = %candidate.display(), "failed to inspect static asset");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    }

    let target = format!(
        "http://{}{}",
        state.backend,
        request
            .uri()
            .path_and_query()
            .map_or("/", |path| path.as_str())
    );
    let uri = match target.parse::<Uri>() {
        Ok(uri) => uri,
        Err(error) => {
            error!(error = %error, "failed to construct runtime request URI");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let host = request.headers().get(header::HOST).cloned();
    strip_hop_headers(request.headers_mut());
    if let Some(host) = host {
        request.headers_mut().insert(header::HOST, host.clone());
        request.headers_mut().insert("x-forwarded-host", host);
    }
    request
        .headers_mut()
        .insert("x-forwarded-proto", HeaderValue::from_static("http"));
    let forwarded_for = match HeaderValue::from_str(&peer.ip().to_string()) {
        Ok(value) => value,
        Err(error) => {
            error!(error = %error, "failed to encode peer address");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    request
        .headers_mut()
        .insert("x-forwarded-for", forwarded_for);
    *request.uri_mut() = uri;
    match state.client.request(request).await {
        Ok(mut response) => {
            strip_hop_headers(response.headers_mut());
            response.map(Body::new)
        }
        Err(error) => {
            error!(error = %error, "Node runtime request failed");
            (StatusCode::BAD_GATEWAY, "Node runtime unavailable").into_response()
        }
    }
}

fn strip_hop_headers(headers: &mut HeaderMap) {
    let named = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(',').map(str::trim))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::json;
    use std::{fs, path::Path};
    use tempfile::TempDir;

    struct ServerTask(tokio::task::JoinHandle<()>);

    impl Drop for ServerTask {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    fn client() -> Client<HttpConnector, Body> {
        Client::builder(TokioExecutor::new()).build_http()
    }

    async fn test_router(assets: &Path) -> Result<(Router, ServerTask)> {
        let listener = TcpListener::bind(loopback(0)).await?;
        let backend = listener.local_addr()?;
        let app =
            Router::new().fallback(|request: Request| async {
                let method = request.method().clone();
                let uri = request.uri().clone();
                let headers = request.headers().clone();
                let bytes = to_bytes(request.into_body(), 1024 * 1024).await.unwrap();
                let mut response = (
                StatusCode::CREATED,
                json!({
                    "method": method.as_str(),
                    "uri": uri.to_string(),
                    "body": String::from_utf8_lossy(&bytes),
                    "host": headers.get("host").unwrap().to_str().unwrap(),
                    "forwarded_host": headers.get("x-forwarded-host").unwrap().to_str().unwrap(),
                    "forwarded_for": headers.get("x-forwarded-for").unwrap().to_str().unwrap(),
                    "forwarded_proto": headers.get("x-forwarded-proto").unwrap().to_str().unwrap(),
                    "hop_header": headers.contains_key("x-hop"),
                }).to_string(),
            ).into_response();
                response
                    .headers_mut()
                    .append("set-cookie", HeaderValue::from_static("a=1"));
                response
                    .headers_mut()
                    .append("set-cookie", HeaderValue::from_static("b=2"));
                response
                    .headers_mut()
                    .insert("location", HeaderValue::from_static("/login"));
                response
                    .headers_mut()
                    .insert("connection", HeaderValue::from_static("x-hop-response"));
                response
                    .headers_mut()
                    .insert("x-hop-response", HeaderValue::from_static("remove"));
                response
            });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let router = Router::new()
            .fallback(route)
            .with_state(AppState {
                manifest: None,
                cache: crate::cache::StaticCache::new(0, 0)?,
                assets: assets.canonicalize()?,
                backend,
                client: client(),
            })
            .layer(axum::Extension(ConnectInfo(loopback(1234))));
        Ok((router, ServerTask(task)))
    }

    fn request(method: &str, uri: &str, body: &str) -> Request {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "localhost:3000")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    #[test]
    fn rejects_encoded_traversal_and_windows_paths() {
        for path in [
            "/../secret",
            "/%2e%2e/secret",
            "/%2e%2e%2fsecret",
            "/%5csecret",
            "/C:%5csecret",
            "/%00secret",
            "/%ff",
            "/./file",
        ] {
            assert!(asset_relative_path(path).is_err(), "{path}");
        }
        assert_eq!(
            asset_relative_path("/images/hello%20world.png").unwrap(),
            Path::new("images").join("hello world.png")
        );
    }

    #[tokio::test]
    async fn serves_only_existing_assets_with_head_and_range_support() -> Result<()> {
        let assets = TempDir::new()?;
        fs::write(assets.path().join("favicon.ico"), "asset-data")?;
        fs::create_dir_all(assets.path().join("_next").join("static"))?;
        fs::write(
            assets.path().join("_next").join("static").join("chunk.js"),
            "javascript",
        )?;
        let (router, _backend) = test_router(assets.path()).await?;
        let response = router
            .clone()
            .oneshot(request("GET", "/favicon.ico?v=1", ""))
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let last_modified = response.headers()["last-modified"].clone();
        assert_eq!(to_bytes(response.into_body(), 100).await?, "asset-data");
        let mut conditional = request("GET", "/favicon.ico", "");
        conditional
            .headers_mut()
            .insert("if-modified-since", last_modified);
        let response = router.clone().oneshot(conditional).await?;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        let response = router
            .clone()
            .oneshot(request("HEAD", "/favicon.ico", ""))
            .await?;
        assert_eq!(response.headers()["content-length"], "10");
        assert!(to_bytes(response.into_body(), 100).await?.is_empty());
        let mut range = request("GET", "/_next/static/chunk.js", "");
        range
            .headers_mut()
            .insert("range", HeaderValue::from_static("bytes=0-3"));
        let response = router.clone().oneshot(range).await?;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(to_bytes(response.into_body(), 100).await?, "java");
        for path in [
            "/",
            "/dashboard",
            "/_next/image?url=x",
            "/missing.png",
            "/_next/static",
        ] {
            let response = router.clone().oneshot(request("GET", path, "")).await?;
            assert_eq!(response.status(), StatusCode::CREATED, "{path}");
        }
        // Non-GET requests must reach Node even when their path names a public asset.
        let response = router
            .oneshot(request("POST", "/favicon.ico", "body"))
            .await?;
        assert_eq!(response.status(), StatusCode::CREATED);
        Ok(())
    }

    #[tokio::test]
    async fn preserves_dynamic_http_semantics_and_removes_hop_headers() -> Result<()> {
        let assets = TempDir::new()?;
        let (router, _backend) = test_router(assets.path()).await?;
        let mut request = request("POST", "/dashboard?q=a%20b", "request body");
        request
            .headers_mut()
            .insert("connection", HeaderValue::from_static("x-hop"));
        request
            .headers_mut()
            .insert("x-hop", HeaderValue::from_static("remove"));
        request
            .headers_mut()
            .insert("x-forwarded-for", HeaderValue::from_static("spoofed"));
        let response = router.oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
        assert_eq!(response.headers()["location"], "/login");
        assert!(!response.headers().contains_key("x-hop-response"));
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
        assert_eq!(body["method"], "POST");
        assert_eq!(body["uri"], "/dashboard?q=a%20b");
        assert_eq!(body["body"], "request body");
        assert_eq!(body["host"], "localhost:3000");
        assert_eq!(body["forwarded_host"], "localhost:3000");
        assert_eq!(body["forwarded_for"], "127.0.0.1");
        assert_eq!(body["forwarded_proto"], "http");
        assert_eq!(body["hop_header"], false);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_invalid_requests_and_reports_unavailable_runtime() -> Result<()> {
        let assets = TempDir::new()?;
        let listener = TcpListener::bind(loopback(0)).await?;
        let backend = listener.local_addr()?;
        drop(listener);
        let router = Router::new()
            .fallback(route)
            .with_state(AppState {
                manifest: None,
                cache: crate::cache::StaticCache::new(0, 0)?,
                assets: assets.path().canonicalize()?,
                backend,
                client: client(),
            })
            .layer(axum::Extension(ConnectInfo(loopback(1234))));
        let response = router
            .clone()
            .oneshot(request("GET", "/%2e%2e/manifest.json", ""))
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let mut upgrade = request("GET", "/dashboard", "");
        upgrade
            .headers_mut()
            .insert("upgrade", HeaderValue::from_static("websocket"));
        let response = router.clone().oneshot(upgrade).await?;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let response = router.oneshot(request("GET", "/dashboard", "")).await?;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        Ok(())
    }

    fn fixture(script: &str) -> Result<TempDir> {
        let output = TempDir::new()?;
        let runtime = output.path().join("runtime");
        let next = runtime.join("node_modules").join("next");
        fs::create_dir_all(next.join("dist").join("bin"))?;
        fs::create_dir_all(runtime.join(".next"))?;
        fs::create_dir_all(output.path().join("static"))?;
        fs::write(next.join("package.json"), "{}")?;
        fs::write(next.join("dist").join("bin").join("next"), script)?;
        fs::write(
            output.path().join("static").join("favicon.ico"),
            "static favicon",
        )?;
        fs::write(output.path().join("manifest.json"), json!({
            "version": 1, "framework": "nextjs",
            "runtime": {
                "type": "node", "command": "node",
                "entrypoint": "runtime/node_modules/next/dist/bin/next",
                "working_directory": "runtime", "args": ["start"]
            },
            "static": {"directory": "static"},
            "deployment": {"id": "test", "repository": "test/repo", "commit": "test", "branch": "main"}
        }).to_string())?;
        Ok(output)
    }

    async fn available_port() -> Result<u16> {
        let listener = TcpListener::bind(loopback(0)).await?;
        Ok(listener.local_addr()?.port())
    }

    #[test]
    fn rejects_unsupported_or_invalid_manifests() -> Result<()> {
        let output = fixture("")?;
        let path = output.path().join("manifest.json");
        let original: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        manifest::load(output.path())?;
        for (pointer, value) in [
            ("/version", json!(2)),
            ("/framework", json!("unknown")),
            ("/runtime/command", json!("cmd")),
            ("/runtime/args", json!(["dev"])),
            ("/runtime/entrypoint", json!("../outside")),
            ("/static/directory", json!("runtime")),
            ("/deployment/id", json!("")),
        ] {
            let mut invalid = original.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            fs::write(&path, invalid.to_string())?;
            assert!(manifest::load(output.path()).is_err(), "{pointer}");
        }
        fs::write(path, "{}")?;
        assert!(manifest::load(output.path()).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn manages_node_child_and_serves_complete_artifact() -> Result<()> {
        let output = fixture(
            r#"
const http = require('node:http');
const fs = require('node:fs');
fs.writeFileSync('node.pid', String(process.pid));
const port = Number(process.argv[process.argv.indexOf('--port') + 1]);
http.createServer((req, res) => {
    res.setHeader('x-node-pid', String(process.pid));
    res.end('dynamic:' + req.url);
}).listen(port, '127.0.0.1');
"#,
        )?;
        let port = available_port().await?;
        let runtime_port = available_port().await?;
        let (stop, stopped) = oneshot::channel::<()>();
        let args = RunArgs {
            output: output.path().to_owned(),
            port,
            runtime_port,
            static_cache_mib: 256,
            static_cache_max_file_mib: 16,
        };
        let task = tokio::spawn(run_with_shutdown(args, async {
            stopped.await.context("test shutdown sender dropped")
        }));
        let result = timeout(Duration::from_secs(15), async {
            loop {
                if TcpStream::connect(loopback(port)).await.is_ok() {
                    break;
                }
                sleep(Duration::from_millis(50)).await;
            }
            let client = client();
            let dynamic = client
                .request(request(
                    "GET",
                    &format!("http://127.0.0.1:{port}/dashboard?test=1"),
                    "",
                ))
                .await?;
            ensure!(dynamic.status() == StatusCode::OK, "dynamic request failed");
            let pid = dynamic.headers()["x-node-pid"].to_str()?.to_owned();
            assert_eq!(
                to_bytes(Body::new(dynamic.into_body()), 4096).await?,
                "dynamic:/dashboard?test=1"
            );
            let static_response = client
                .request(request(
                    "GET",
                    &format!("http://127.0.0.1:{port}/favicon.ico"),
                    "",
                ))
                .await?;
            assert!(!static_response.headers().contains_key("x-node-pid"));
            assert_eq!(
                to_bytes(Body::new(static_response.into_body()), 4096).await?,
                "static favicon"
            );
            Ok::<_, anyhow::Error>(pid)
        })
        .await;
        let _ = stop.send(());
        timeout(Duration::from_secs(10), task).await???;
        let pid = result??;
        let _public = TcpListener::bind(loopback(port)).await?;
        let _internal = TcpListener::bind(loopback(runtime_port)).await?;
        let status = std::process::Command::new("node")
            .args(["-e", "try { process.kill(Number(process.argv[1]), 0); process.exit(1); } catch (e) { if (e.code !== 'ESRCH') throw e; }", &pid])
            .status()?;
        ensure!(status.success(), "Node child is still alive after shutdown");
        Ok(())
    }

    #[tokio::test]
    async fn reports_node_startup_failure_and_port_conflicts() -> Result<()> {
        let output = fixture("process.exit(23);")?;
        let port = available_port().await?;
        let runtime_port = available_port().await?;
        let args = RunArgs {
            output: output.path().to_owned(),
            port,
            runtime_port,
            static_cache_mib: 256,
            static_cache_max_file_mib: 16,
        };
        let error = run_with_shutdown(args, std::future::pending())
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("exited before becoming ready"));
        assert!(message.contains("23"), "{message}");
        let _public = TcpListener::bind(loopback(port)).await?;
        let internal = TcpListener::bind(loopback(runtime_port)).await?;
        let args = RunArgs {
            output: output.path().to_owned(),
            port: available_port().await?,
            runtime_port,
            static_cache_mib: 256,
            static_cache_max_file_mib: 16,
        };
        let error = run_with_shutdown(args, std::future::pending())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("already in use"));
        drop(internal);
        let args = RunArgs {
            output: output.path().to_owned(),
            port: 3000,
            runtime_port: 3000,
            static_cache_mib: 256,
            static_cache_max_file_mib: 16,
        };
        let error = run_with_shutdown(args, std::future::pending())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("must be different"));
        Ok(())
    }

    #[tokio::test]
    async fn reports_node_exit_after_startup_and_releases_public_port() -> Result<()> {
        let output = fixture(
            r#"
const http = require('node:http');
const port = Number(process.argv[process.argv.indexOf('--port') + 1]);
http.createServer((req, res) => res.end('ready')).listen(port, '127.0.0.1', () => {
    setTimeout(() => process.exit(24), 1000);
});
"#,
        )?;
        let port = available_port().await?;
        let runtime_port = available_port().await?;
        let error = timeout(
            Duration::from_secs(10),
            run_with_shutdown(
                RunArgs {
                    output: output.path().to_owned(),
                    port,
                    runtime_port,
                    static_cache_mib: 256,
                    static_cache_max_file_mib: 16,
                },
                std::future::pending(),
            ),
        )
        .await?
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("exited unexpectedly"), "{message}");
        assert!(message.contains("24"), "{message}");
        let _listener = TcpListener::bind(loopback(port)).await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "installs and builds a real Next.js fixture; requires npm, Node and network access"]
    async fn runs_relocated_nextjs_application() -> Result<()> {
        run_relocated_nextjs_application(crate::PackageManager::Npm).await
    }

    #[tokio::test]
    #[ignore = "installs and builds a pnpm Next.js fixture; requires npm, Node and network access"]
    async fn runs_relocated_pnpm_nextjs_application() -> Result<()> {
        run_relocated_nextjs_application(crate::PackageManager::Pnpm).await
    }

    async fn run_relocated_nextjs_application(
        package_manager: crate::PackageManager,
    ) -> Result<()> {
        let workspace = TempDir::new()?;
        let project = workspace.path().join("repo");
        fs::create_dir_all(project.join("app").join("api").join("echo"))?;
        fs::create_dir_all(project.join("app").join("about"))?;
        fs::create_dir_all(project.join("public"))?;
        fs::write(
            project.join("package.json"),
            json!({
                "name": "meshscale-runtime-test", "private": true, "type": "module",
                "scripts": {"build": "next build"},
                "dependencies": {
                    "next": if matches!(package_manager, crate::PackageManager::Pnpm) { "16.3.4" } else { "^16" },
                    "react": "^19", "react-dom": "^19"
                }
            })
            .to_string(),
        )?;
        fs::write(
            project.join("next.config.mjs"),
            "export default { poweredByHeader: false };",
        )?;
        fs::write(
            project.join("app").join("layout.js"),
            "export default function Layout({children}) { return <html><body>{children}</body></html>; }",
        )?;
        fs::write(
            project.join("app").join("page.js"),
            "export const dynamic = 'force-dynamic'; export default function Page() { return <div>MeshScale dynamic fixture</div>; }",
        )?;
        fs::write(
            project.join("app").join("about").join("page.js"),
            "export default function About() { return <div>MeshScale generated page</div>; }",
        )?;
        fs::write(
            project
                .join("app")
                .join("api")
                .join("echo")
                .join("route.js"),
            "export async function POST(req) { return new Response(new URL(req.url).search + ':' + await req.text(), {status: 201}); }",
        )?;
        fs::write(
            project.join("public").join("asset.txt"),
            "Next.js public fixture",
        )?;
        let commands = match package_manager {
            crate::PackageManager::Pnpm => vec![
                vec![
                    "exec",
                    "--yes",
                    "--package=pnpm@10",
                    "--",
                    "pnpm",
                    "install",
                ],
                vec![
                    "exec",
                    "--yes",
                    "--package=pnpm@10",
                    "--",
                    "pnpm",
                    "run",
                    "build",
                ],
            ],
            _ => vec![
                vec!["install", "--no-audit", "--no-fund"],
                vec!["run", "build"],
            ],
        };
        for args in commands {
            let status =
                tokio::process::Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" })
                    .args(args)
                    .current_dir(&project)
                    .env("NEXT_TELEMETRY_DISABLED", "1")
                    .kill_on_drop(true)
                    .status()
                    .await?;
            ensure!(
                status.success(),
                "Next.js fixture install/build failed: {status}"
            );
        }
        let output = artifact::create_output(
            &project,
            &crate::BuildMetadata {
                version: 2,
                framework: crate::Framework::NextJs,
                package_manager,
                build_id: "real-next".to_owned(),
                repository: "test/repo".to_owned(),
                commit: "1234567890abcdef1234567890abcdef12345678".to_owned(),
                branch: "main".to_owned(),
                org_id: Some("org_test".into()),
                project_id: Some("project_test".into()),
            },
            &workspace.path().join("output"),
        )?;
        ensure!(
            !output.join("runtime").join("next.config.mjs").exists(),
            "source configuration was copied"
        );
        ensure!(
            !output
                .join("runtime")
                .join(".next")
                .join("next-server.js.nft.json")
                .exists(),
            "build-only server trace was copied"
        );
        fs::remove_dir_all(&project)?;
        let port = available_port().await?;
        let runtime_port = available_port().await?;
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(run_with_shutdown(
            RunArgs {
                output: output.clone(),
                port,
                runtime_port,
                static_cache_mib: 256,
                static_cache_max_file_mib: 16,
            },
            async { stopped.await.context("test shutdown sender dropped") },
        ));
        let result = timeout(Duration::from_secs(40), async {
            loop {
                if TcpStream::connect(loopback(port)).await.is_ok() {
                    break;
                }
                sleep(Duration::from_millis(50)).await;
            }
            let client = client();
            let root = client
                .request(request("GET", &format!("http://127.0.0.1:{port}/"), ""))
                .await?;
            ensure!(
                root.status() == StatusCode::OK,
                "Next.js root returned {}",
                root.status()
            );
            ensure!(
                !root.headers().contains_key("x-powered-by"),
                "build-time Next.js configuration was not preserved"
            );
            let html = to_bytes(Body::new(root.into_body()), 1024 * 1024).await?;
            ensure!(
                String::from_utf8_lossy(&html).contains("MeshScale dynamic fixture"),
                "Next.js HTML missing fixture"
            );
            let about = client
                .request(request(
                    "GET",
                    &format!("http://127.0.0.1:{port}/about"),
                    "",
                ))
                .await?;
            ensure!(
                about.status() == StatusCode::OK,
                "generated Next.js page returned {}",
                about.status()
            );
            ensure!(
                about.headers().contains_key("etag"),
                "generated HTML was not served by the static manifest"
            );
            let html = to_bytes(Body::new(about.into_body()), 1024 * 1024).await?;
            ensure!(
                String::from_utf8_lossy(&html).contains("MeshScale generated page"),
                "generated Next.js page was not preserved"
            );
            let echo = client
                .request(request(
                    "POST",
                    &format!("http://127.0.0.1:{port}/api/echo?q=1"),
                    "payload",
                ))
                .await?;
            ensure!(
                echo.status() == StatusCode::CREATED,
                "Next.js API returned {}",
                echo.status()
            );
            assert_eq!(
                to_bytes(Body::new(echo.into_body()), 4096).await?,
                "?q=1:payload"
            );
            let asset = client
                .request(request(
                    "GET",
                    &format!("http://127.0.0.1:{port}/asset.txt"),
                    "",
                ))
                .await?;
            assert_eq!(
                to_bytes(Body::new(asset.into_body()), 4096).await?,
                "Next.js public fixture"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await;
        let _ = stop.send(());
        timeout(Duration::from_secs(10), task).await???;
        result??;
        let publication = crate::upload::tests::publish_real_fixture(&output).await?;
        let extracted = workspace.path().join("server-only");
        let mut archive = zip::ZipArchive::new(fs::File::open(&publication.archive.path)?)?;
        archive.extract(&extracted)?;
        ensure!(
            !extracted.join("static").exists(),
            "ZIP contains top-level static"
        );
        let metadata = manifest::load_server(&extracted)?;
        ensure!(
            metadata.r#static.storage.is_some(),
            "archived manifest missing publication binding"
        );
        let fallback_file =
            walkdir::WalkDir::new(extracted.join("runtime").join(".next").join("static"))
                .into_iter()
                .collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .find(|entry| {
                    entry.file_type().is_file()
                        && entry.path().extension().is_some_and(|ext| ext == "js")
                })
                .context("bundle has no Next.js static fallback asset")?;
        let fallback_path = format!(
            "/_next/static/{}",
            fallback_file
                .path()
                .strip_prefix(extracted.join("runtime").join(".next").join("static"))?
                .to_string_lossy()
                .replace('\\', "/")
        );
        let port = available_port().await?;
        let runtime_port = available_port().await?;
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(run_with_shutdown(
            RunArgs {
                output: extracted,
                port,
                runtime_port,
                static_cache_mib: 256,
                static_cache_max_file_mib: 16,
            },
            async { stopped.await.context("test shutdown sender dropped") },
        ));
        let result = timeout(Duration::from_secs(40), async {
            loop {
                if TcpStream::connect(loopback(port)).await.is_ok() {
                    break;
                }
                sleep(Duration::from_millis(50)).await;
            }
            let client = client();
            for path in ["/", "/about", fallback_path.as_str()] {
                let response = client
                    .request(request(
                        "GET",
                        &format!("http://127.0.0.1:{port}{path}"),
                        "",
                    ))
                    .await?;
                ensure!(
                    response.status() == StatusCode::OK,
                    "extracted server failed for {path}: {}",
                    response.status()
                );
                to_bytes(Body::new(response.into_body()), 16 * 1024 * 1024).await?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await;
        let _ = stop.send(());
        timeout(Duration::from_secs(10), task).await???;
        result??;
        Ok(())
    }
}
