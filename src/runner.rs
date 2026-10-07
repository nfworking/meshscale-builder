use anyhow::{Context, Result, bail, ensure};
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use clap::Args;
use futures_util::stream::unfold;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, ChildStdin},
    sync::{Mutex, mpsc, oneshot},
    time::sleep,
};
use tower::ServiceExt;
use tower_http::services::ServeFile;
use tracing::{error, info};

use crate::{artifact, manifest};

const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;
const MAX_WIRE_HEADER: usize = 1024 * 1024;

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

#[derive(Debug, Serialize, Deserialize)]
struct WireHeader {
    kind: String,
    #[serde(default)]
    id: u64,
    #[serde(default)]
    status: u16,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    length: usize,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug)]
struct WireFrame {
    header: WireHeader,
    body: Vec<u8>,
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    header: &WireHeader,
    body: &[u8],
) -> Result<()> {
    let json = serde_json::to_vec(header).context("failed to encode function protocol header")?;
    ensure!(
        json.len() <= MAX_WIRE_HEADER,
        "function protocol header is too large"
    );
    ensure!(
        body.len() == header.length,
        "function protocol body length mismatch"
    );
    writer
        .write_u32(json.len() as u32)
        .await
        .context("failed to write function protocol header length")?;
    writer
        .write_u32(body.len() as u32)
        .await
        .context("failed to write function protocol body length")?;
    writer
        .write_all(&json)
        .await
        .context("failed to write function protocol header")?;
    writer
        .write_all(body)
        .await
        .context("failed to write function protocol body")?;
    writer
        .flush()
        .await
        .context("failed to flush function protocol")?;
    Ok(())
}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<WireFrame> {
    let header_len = reader
        .read_u32()
        .await
        .context("function protocol closed while reading header length")?
        as usize;
    let body_len = reader
        .read_u32()
        .await
        .context("function protocol closed while reading body length")? as usize;
    ensure!(
        header_len <= MAX_WIRE_HEADER,
        "function protocol header is too large"
    );
    ensure!(
        body_len <= MAX_REQUEST_BODY,
        "function protocol frame body is too large"
    );
    let mut json = vec![0; header_len];
    reader
        .read_exact(&mut json)
        .await
        .context("function protocol closed while reading header")?;
    let header: WireHeader =
        serde_json::from_slice(&json).context("invalid function protocol header")?;
    ensure!(
        header.length == body_len,
        "function protocol declared body length does not match frame"
    );
    let mut body = vec![0; body_len];
    reader
        .read_exact(&mut body)
        .await
        .context("function protocol closed while reading body")?;
    Ok(WireFrame { header, body })
}

struct Worker {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<WireFrame, String>>>>,
    next_id: AtomicU64,
    last_used: Mutex<Instant>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Worker {
    async fn start(cwd: &std::path::Path, entrypoint: &std::path::Path) -> Result<Arc<Self>> {
        let mut command = tokio::process::Command::new("node");
        crate::upload::remove_credentials(command.as_std_mut());
        let mut child = command
            .arg(entrypoint)
            .current_dir(cwd)
            .env("NODE_ENV", "production")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("failed to start Node function worker")?;
        let stdin = child
            .stdin
            .take()
            .context("Node function worker stdin was not piped")?;
        let stdout = child
            .stdout
            .take()
            .context("Node function worker stdout was not piped")?;
        let worker = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            last_used: Mutex::new(Instant::now()),
            reader: Mutex::new(None),
        });
        let (ready_tx, ready_rx) = oneshot::channel();
        let mut ready_tx = Some(ready_tx);
        let reader_worker = worker.clone();
        let reader = tokio::spawn(async move {
            let mut stdout = stdout;
            let result = loop {
                match read_frame(&mut stdout).await {
                    Ok(frame) if frame.header.kind == "ready" => {
                        if let Some(sender) = ready_tx.take() {
                            let _ = sender.send(Ok(()));
                        }
                    }
                    Ok(frame) => {
                        let id = frame.header.id;
                        let sender = reader_worker.pending.lock().await.get(&id).cloned();
                        if let Some(sender) = sender {
                            if sender.send(Ok(frame)).await.is_err() {
                                reader_worker.pending.lock().await.remove(&id);
                            }
                        }
                    }
                    Err(error) => break Err(error),
                }
            };
            let message = match result {
                Ok(()) => "function worker protocol closed".to_owned(),
                Err(error) => format!("{error:#}"),
            };
            let pending = std::mem::take(&mut *reader_worker.pending.lock().await);
            for (_, sender) in pending {
                let _ = sender.send(Err(message.clone())).await;
            }
        });
        *worker.reader.lock().await = Some(reader);

        match tokio::time::timeout(Duration::from_secs(30), ready_rx).await {
            Ok(Ok(Ok(()))) => {
                info!("started direct Node function worker");
                Ok(worker)
            }
            Ok(Ok(Err(error))) => {
                worker.shutdown().await;
                Err(error)
            }
            Ok(Err(_)) => {
                worker.shutdown().await;
                bail!("Node function worker exited before sending its ready signal")
            }
            Err(_) => {
                worker.shutdown().await;
                bail!("Node function worker did not become ready within 30 seconds")
            }
        }
    }

    async fn invoke(self: &Arc<Self>, request: Request, active: ActiveRequest) -> Result<Response> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, mut receiver) = mpsc::channel(32);
        self.pending.lock().await.insert(id, sender);
        *self.last_used.lock().await = Instant::now();

        let (parts, body) = request.into_parts();
        let body = axum::body::to_bytes(body, MAX_REQUEST_BODY)
            .await
            .context("failed to read request body for function invocation")?;
        let headers = parts
            .headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect::<Vec<_>>();

        let header = WireHeader {
            kind: "request".to_owned(),
            id,
            status: 0,
            headers,
            method: Some(parts.method.to_string()),
            uri: Some(parts.uri.to_string()),
            length: body.len(),
            error: None,
        };
        if let Err(error) = self.send(&header, &body).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }

        let first = receiver
            .recv()
            .await
            .context("function worker closed before returning response headers")?
            .map_err(|error| anyhow::anyhow!(error))?;
        if first.header.kind == "error" {
            self.pending.lock().await.remove(&id);
            bail!(
                "function worker returned an invocation error: {}",
                first
                    .header
                    .error
                    .unwrap_or_else(|| "unknown error".to_owned())
            );
        }
        ensure!(
            first.header.kind == "headers",
            "function worker returned unexpected response frame"
        );
        let status = StatusCode::from_u16(first.header.status)
            .context("function worker returned invalid status code")?;
        let mut response = Response::builder().status(status);
        for (name, value) in &first.header.headers {
            response = response.header(name, value);
        }

        let worker = self.clone();
        let stream = unfold((receiver, active), move |(mut receiver, active)| {
            let worker = worker.clone();
            async move {
                loop {
                    match receiver.recv().await {
                        Some(Ok(frame)) if frame.header.kind == "chunk" => {
                            return Some((
                                Ok::<_, std::io::Error>(axum::body::Bytes::from(frame.body)),
                                (receiver, active),
                            ));
                        }
                        Some(Ok(frame)) if frame.header.kind == "end" => {
                            worker.pending.lock().await.remove(&id);
                            drop(active);
                            return None;
                        }
                        Some(Ok(frame)) if frame.header.kind == "error" => {
                            let error = frame
                                .header
                                .error
                                .unwrap_or_else(|| "function worker invocation failed".to_owned());
                            worker.pending.lock().await.remove(&id);
                            return Some((Err(std::io::Error::other(error)), (receiver, active)));
                        }
                        Some(Ok(_)) => continue,
                        Some(Err(error)) => {
                            worker.pending.lock().await.remove(&id);
                            return Some((Err(std::io::Error::other(error)), (receiver, active)));
                        }
                        None => {
                            worker.pending.lock().await.remove(&id);
                            return Some((
                                Err(std::io::Error::other(
                                    "function worker closed before ending the response",
                                )),
                                (receiver, active),
                            ));
                        }
                    }
                }
            }
        });
        response
            .body(Body::from_stream(stream))
            .context("failed to construct function response")
    }

    async fn send(&self, header: &WireHeader, body: &[u8]) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        write_frame(&mut *stdin, header, body).await
    }

    async fn is_alive(&self) -> Result<bool> {
        Ok(self
            .child
            .lock()
            .await
            .try_wait()
            .context("failed to inspect function worker")?
            .is_none())
    }

    async fn shutdown(&self) {
        if let Some(reader) = self.reader.lock().await.take() {
            reader.abort();
        }
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

struct FunctionManager {
    output: PathBuf,
    cwd: PathBuf,
    entrypoint: PathBuf,
    idle_timeout: Duration,
    worker: Mutex<Option<Arc<Worker>>>,
    active_requests: Arc<AtomicUsize>,
}

impl FunctionManager {
    async fn invoke(&self, request: Request) -> Result<Response> {
        let active = ActiveRequest::new(self.active_requests.clone());
        let worker = self.ensure_worker().await?;
        let response = worker.invoke(request, active).await;
        *worker.last_used.lock().await = Instant::now();
        response
    }

    async fn ensure_worker(&self) -> Result<Arc<Worker>> {
        let mut slot = self.worker.lock().await;
        if let Some(existing) = slot.as_ref() {
            if existing.is_alive().await? {
                return Ok(existing.clone());
            }
            existing.shutdown().await;
            *slot = None;
        }
        let worker = Worker::start(&self.cwd, &self.entrypoint).await?;
        *slot = Some(worker.clone());
        info!(project = %self.output.display(), "function worker ready");
        Ok(worker)
    }

    async fn reap_idle(&self) {
        let mut slot = self.worker.lock().await;
        let Some(worker) = slot.as_ref() else {
            return;
        };
        if self.active_requests.load(Ordering::Acquire) != 0 {
            return;
        }
        if worker.last_used.lock().await.elapsed() < self.idle_timeout {
            return;
        }
        info!(project = %self.output.display(), "stopping idle function worker");
        worker.shutdown().await;
        *slot = None;
    }

    async fn shutdown(&self) {
        let mut slot = self.worker.lock().await;
        if let Some(worker) = slot.take() {
            worker.shutdown().await;
        }
    }
}

struct ActiveRequest {
    counter: Arc<AtomicUsize>,
}

impl ActiveRequest {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { counter }
    }
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
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
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(5)).await;
                function.reap_idle().await;
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port))
        .await
        .with_context(|| format!("failed to bind local edge port {}", args.port))?;
    let router = Router::new().fallback(route).with_state(state.clone());
    info!(url = %format!("http://127.0.0.1:{}", args.port), "MeshScale local edge ready");
    info!(
        "Node function workers are lazy: no Node process is started until a dynamic request arrives"
    );

    axum::serve(listener, router.into_make_service())
        .with_graceful_shutdown(async move {
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
            active_requests: Arc::new(AtomicUsize::new(0)),
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

async fn dispatch(project: Arc<ProjectRuntime>, request: Request) -> Response {
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
        let Some(_object) = project.manifest.r#static.objects.get(object_key) else {
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
