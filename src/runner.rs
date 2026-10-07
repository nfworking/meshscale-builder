use anyhow::{Context, Result, bail, ensure};
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use clap::Args;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::{TcpListener, TcpStream},
    process::Child,
    sync::Mutex,
    time::sleep,
};
use tower::ServiceExt;
use tower_http::services::ServeFile;
use tracing::{error, info};

use crate::{artifact, manifest};

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Run one output directory when --project is not supplied.
    pub output: Option<PathBuf>,
    /// Map a local hostname to an output directory: app.localhost=./app-output.
    #[arg(long = "project", value_name = "HOST=OUTPUT")]
    pub projects: Vec<String>,
    /// Public HTTP port for the local MeshScale edge.
    #[arg(long, default_value_t = 3000, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: u16,
    /// Idle time before a warm function worker is stopped.
    #[arg(long, default_value_t = 300)]
    pub idle_timeout_secs: u64,
}

struct Worker {
    addr: SocketAddr,
    child: Child,
    last_used: Instant,
}

struct FunctionManager {
    output: PathBuf,
    cwd: PathBuf,
    entrypoint: PathBuf,
    idle_timeout: Duration,
    worker: Mutex<Option<Worker>>,
}

impl FunctionManager {
    async fn invoke(&self, mut request: Request) -> Result<Response> {
        let addr = self.ensure_worker().await?;
        let uri = format!(
            "http://{}{}",
            addr,
            request.uri().path_and_query().map_or("/", Uri::as_str)
        )
        .parse::<Uri>()
        .context("failed to construct function invocation URI")?;

        let host = request.headers().get(header::HOST).cloned();
        strip_hop_headers(request.headers_mut());
        if let Some(host) = host {
            request.headers_mut().insert(header::HOST, host.clone());
            request.headers_mut().insert("x-forwarded-host", host);
        }
        request
            .headers_mut()
            .insert("x-forwarded-proto", HeaderValue::from_static("http"));
        *request.uri_mut() = uri;

        let client = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
        let response = client
            .request(request)
            .await
            .context("function worker request failed")?;
        Ok(response.map(Body::new))
    }

    async fn ensure_worker(&self) -> Result<SocketAddr> {
        let mut worker = self.worker.lock().await;

        if let Some(existing) = worker.as_mut() {
            if existing
                .child
                .try_wait()
                .context("failed to inspect function worker")?
                .is_none()
            {
                existing.last_used = Instant::now();
                return Ok(existing.addr);
            }
            *worker = None;
        }

        let probe = TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to reserve a local function port")?;
        let addr = probe.local_addr()?;
        drop(probe);

        let entrypoint = self
            .entrypoint
            .strip_prefix(&self.cwd)
            .context("function entrypoint is outside runtime working directory")?;

        let mut command = tokio::process::Command::new("node");
        crate::upload::remove_credentials(command.as_std_mut());
        let mut child = command
            .arg(entrypoint)
            .current_dir(&self.cwd)
            .env("NODE_ENV", "production")
            .env("PORT", addr.port().to_string())
            .env("HOSTNAME", "127.0.0.1")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("failed to start Node function worker")?;

        wait_for_worker(&mut child, addr).await?;

        info!(
            project = %self.output.display(),
            address = %addr,
            "started local function worker"
        );

        *worker = Some(Worker {
            addr,
            child,
            last_used: Instant::now(),
        });

        Ok(addr)
    }

    async fn reap_idle(&self) {
        let mut worker = self.worker.lock().await;
        let Some(existing) = worker.as_mut() else {
            return;
        };

        if existing.last_used.elapsed() < self.idle_timeout {
            return;
        }

        info!(
            project = %self.output.display(),
            "stopping idle function worker"
        );
        let _ = existing.child.kill().await;
        let _ = existing.child.wait().await;
        *worker = None;
    }

    async fn shutdown(&self) {
        let mut worker = self.worker.lock().await;
        if let Some(existing) = worker.as_mut() {
            let _ = existing.child.kill().await;
            let _ = existing.child.wait().await;
        }
        *worker = None;
    }
}

struct ProjectRuntime {
    host: String,
    output: PathBuf,
    assets: PathBuf,
    manifest: Arc<manifest::Manifest>,
    function: Arc<FunctionManager>,
}

#[derive(Clone)]
struct AppState {
    projects: Arc<HashMap<String, Arc<ProjectRuntime>>>,
    default: Option<Arc<ProjectRuntime>>,
}

pub fn run(args: RunArgs) -> Result<()> {
    tokio::runtime::Runtime::new()
        .context("failed to create runner executor")?
        .block_on(run_async(args))
}

async fn run_async(args: RunArgs) -> Result<()> {
    let projects = load_projects(&args)?;
    let default = if projects.len() == 1 && args.projects.is_empty() {
        projects.values().next().cloned()
    } else {
        None
    };

    let state = AppState {
        projects: Arc::new(projects),
        default,
    };

    for project in state.projects.values() {
        info!(host = %project.host, "loaded local deployment");
        let function = project.function.clone();
        let interval = Duration::from_secs(5);
        tokio::spawn(async move {
            loop {
                sleep(interval).await;
                function.reap_idle().await;
            }
        });
    }

    let listener = TcpListener::bind(("127.0.0.1", args.port))
        .await
        .with_context(|| format!("failed to bind local edge port {}", args.port))?;

    let router = Router::new().fallback(route).with_state(state.clone());

    info!(url = %format!("http://127.0.0.1:{}", args.port), "MeshScale local edge ready");
    info!(
        "Node function workers are lazy: no Node process is started until a dynamic request arrives"
    );

    axum::serve(listener, router.into_make_service())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            for project in state.projects.values() {
                project.function.shutdown().await;
            }
        })
        .await
    .context("local MeshScale edge failed")?;

    Ok(())
}

fn load_projects(args: &RunArgs) -> Result<HashMap<String, Arc<ProjectRuntime>>> {
    ensure!(
        args.output.is_some() || !args.projects.is_empty(),
        "provide an output directory or at least one --project HOST=OUTPUT"
    );
    ensure!(
        args.output.is_none() || args.projects.is_empty(),
        "use either the positional output directory or --project, not both"
    );

    let specs = if let Some(output) = &args.output {
        vec![(String::from("*"), output.clone())]
    } else {
        args.projects
            .iter()
            .map(|spec| {
                let (host, output) = spec
                    .split_once('=')
                    .context("--project must use HOST=OUTPUT")?;
                ensure!(!host.trim().is_empty(), "--project host cannot be empty");
                ensure!(
                    !output.trim().is_empty(),
                    "--project output cannot be empty"
                );
                Ok((host.to_ascii_lowercase(), PathBuf::from(output)))
            })
            .collect::<Result<Vec<_>>>()?
    };

    let mut projects = HashMap::new();

    for (host, output) in specs {
        let manifest = Arc::new(manifest::load_server(&output)?);
        artifact::validate_server_output(&output)?;
        if manifest.version == 2 {
            crate::static_output::verify_inventory(&output, &manifest)?;
        }

        let cwd = manifest::resolve_path(&output, &manifest.runtime.working_directory)?;
        let entrypoint = manifest::resolve_path(&output, &manifest.runtime.entrypoint)?;
        let assets = manifest::resolve_path(&output, &manifest.r#static.directory)?;

        ensure!(
            manifest.runtime.entrypoint == PathBuf::from("runtime/function-entry.cjs"),
            "runner requires a MeshScale function artifact"
        );

        let function = Arc::new(FunctionManager {
            output: output.clone(),
            cwd,
            entrypoint,
            idle_timeout: Duration::from_secs(args.idle_timeout_secs),
            worker: Mutex::new(None),
        });

        let key = if host == "*" {
            "*".to_owned()
        } else {
            host.trim_end_matches('.').to_ascii_lowercase()
        };

        if projects.contains_key(&key) {
            bail!("duplicate runner hostname: {key}");
        }

        projects.insert(
            key.clone(),
            Arc::new(ProjectRuntime {
                host: key,
                output,
                assets,
                manifest,
                function,
            }),
        );
    }

    Ok(projects)
}

async fn route(State(state): State<AppState>, request: Request) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .trim_end_matches('.')
        .to_ascii_lowercase();

    let project = state
        .projects
        .get(&host)
        .cloned()
        .or_else(|| state.projects.get("*").cloned());

    let Some(project) = project else {
        return (StatusCode::MISDIRECTED_REQUEST, "unknown MeshScale project").into_response();
    };

    dispatch(project, request).await
}

async fn dispatch(project: Arc<ProjectRuntime>, mut request: Request) -> Response {
    if request.headers().contains_key(header::UPGRADE) {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "HTTP upgrades are not supported by the local runner",
        )
            .into_response();
    }

    let selected = match project
        .manifest
        .routing
        .as_ref()
        .context("missing routing contract")
        .and_then(|routing| routing.select(request.method(), request.uri(), request.headers()))
    {
        Ok(value) => value,
        Err(error) => {
            error!(error = %error, "invalid routed request");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    if let Some(object_key) = selected {
        let Some(object) = project.manifest.r#static.objects.get(object_key) else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        let candidate = project.assets.join(object_key);

        match tokio::fs::canonicalize(&candidate).await {
            Ok(path) if path.starts_with(&project.assets) && path.is_file() => {
                match ServeFile::new(path).oneshot(request).await {
                    Ok(response) => return response.map(Body::new).into_response(),
                    Err(error) => match error {},
                }
            }
            Ok(_) => return StatusCode::FORBIDDEN.into_response(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                error!(error = %error, "failed to inspect static asset");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }

        if request.method() == Method::GET || request.method() == Method::HEAD {
            return StatusCode::NOT_FOUND.into_response();
        }
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    match project.function.invoke(request).await {
        Ok(response) => response,
        Err(error) => {
            error!(error = %error, "function invocation failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

async fn wait_for_worker(child: &mut Child, addr: SocketAddr) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        if Instant::now() >= deadline {
            let _ = child.kill().await;
            bail!("Node function worker did not become ready within 30 seconds");
        }

        if let Ok(stream) = TcpStream::connect(addr).await {
            drop(stream);
            return Ok(());
        }

        if let Some(status) = child
            .try_wait()
            .context("failed to inspect Node function worker")?
        {
            bail!("Node function worker exited during startup with {status}");
        }

        sleep(Duration::from_millis(25)).await;
    }
}

fn strip_hop_headers(headers: &mut axum::http::HeaderMap) {
    for name in [
        header::CONNECTION,
        header::KEEP_ALIVE,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
}
