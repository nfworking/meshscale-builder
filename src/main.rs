use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use git2::{build::RepoBuilder, Cred, CredentialType, FetchOptions, Oid, RemoteCallbacks, Repository};
use serde::Serialize;
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{self},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};
use tempfile::TempDir;
use tracing::{info, warn};
use walkdir::WalkDir;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

#[derive(Parser, Debug)]
#[command(name = "meshscale-builder", version, about = "MeshScale application builder")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Clone, detect, install, build and package a GitHub project.
    Deploy(DeployArgs),
}

#[derive(clap::Args, Debug)]
struct DeployArgs {
    /// Directory containing package.json, relative to the repository root.
    #[arg(long, default_value = ".")]
    dir: String,
    /// GitHub owner/username.
    #[arg(long = "git-username")]
    git_username: String,
    /// Repository name.
    #[arg(long = "git-repo")]
    git_repo: String,
    /// Exact commit SHA to build.
    #[arg(long = "git-hash")]
    git_hash: String,
    /// Branch to clone.
    #[arg(long = "git-branch")]
    git_branch: String,
    /// GitHub personal access token used for repository authentication.
    #[arg(long = "access-token")]
    access_token: String,
    /// MeshScale build identifier.
    #[arg(long = "build-id")]
    build_id: String,
}

#[derive(Debug, Serialize)]
struct BuildResult {
    status: &'static str,
    build_id: String,
    framework: Option<String>,
    package_manager: Option<String>,
    artifact_path: Option<String>,
    artifact_size_bytes: Option<u64>,
    build_time_ms: u128,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum Framework {
    NextJs,
}

impl Framework {
    fn as_str(self) -> &'static str {
        match self {
            Self::NextJs => "nextjs",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PackageManager {
    Npm,
    Pnpm,
    Yarn,
}

impl PackageManager {
    fn executable(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
        }
    }

    fn install_args(self, has_lockfile: bool) -> Vec<&'static str> {
        match (self, has_lockfile) {
            (Self::Npm, true) => vec!["ci"],
            (Self::Npm, false) => vec!["install"],
            (Self::Pnpm, _) => vec!["install", "--frozen-lockfile"],
            (Self::Yarn, _) => vec!["install", "--frozen-lockfile"],
        }
    }

    fn build_args(self) -> Vec<&'static str> {
        match self {
            Self::Npm | Self::Pnpm => vec!["run", "build"],
            Self::Yarn => vec!["build"],
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
        }
    }
}

struct BuildContext {
    project_dir: PathBuf,
    package_json: Value,
    framework: Framework,
    package_manager: PackageManager,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "meshscale_builder=info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Deploy(args) => run_deploy(args),
    };

    match result {
        Ok(result) => {
            println!("{}", serde_json::to_string_pretty(&result).expect("result serializes"));
            if result.status != "success" {
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("builder failed: {error:#}");
            std::process::exit(1);
        }
    }
}

fn run_deploy(args: DeployArgs) -> Result<BuildResult> {
    let started = Instant::now();

    match build_application(&args) {
        Ok((framework, package_manager, artifact_path, artifact_size)) => Ok(BuildResult {
            status: "success",
            build_id: args.build_id,
            framework: Some(framework.as_str().to_owned()),
            package_manager: Some(package_manager.as_str().to_owned()),
            artifact_path: Some(artifact_path.display().to_string()),
            artifact_size_bytes: Some(artifact_size),
            build_time_ms: started.elapsed().as_millis(),
            error: None,
        }),
        Err(error) => Ok(BuildResult {
            status: "failed",
            build_id: args.build_id,
            framework: None,
            package_manager: None,
            artifact_path: None,
            artifact_size_bytes: None,
            build_time_ms: started.elapsed().as_millis(),
            error: Some(error.to_string()),
        }),
    }
}

fn build_application(args: &DeployArgs) -> Result<(Framework, PackageManager, PathBuf, u64)> {
    validate_args(args)?;

    let workspace = TempDir::new().context("failed to create temporary build workspace")?;
    let repo_dir = workspace.path().join("repo");

    info!(build_id = %args.build_id, repository = %format!("{}/{}", args.git_username, args.git_repo), "cloning repository");
    clone_repository(args, &repo_dir)?;
    checkout_commit(&repo_dir, &args.git_hash)?;

    let project_dir = resolve_project_dir(&repo_dir, &args.dir)?;
    let package_json = load_package_json(&project_dir.join("package.json"))?;
    let framework = detect_framework(&package_json)?;
    let package_manager = detect_package_manager(&project_dir, &package_json)?;

    info!(
        framework = framework.as_str(),
        package_manager = package_manager.as_str(),
        project_dir = %project_dir.display(),
        "detected build configuration"
    );

    let context = BuildContext {
        project_dir,
        package_json,
        framework,
        package_manager,
    };

    install_dependencies(&context)?;
    run_build(&context)?;

    let next_dir = context.project_dir.join(".next");
    if !next_dir.is_dir() {
        bail!("build completed but .next directory was not produced");
    }

    let output_path = artifact_output_path(&args.build_id)?;
    create_artifact(&context, &next_dir, &output_path)?;

    let size = fs::metadata(&output_path)
        .with_context(|| format!("failed to stat artifact {}", output_path.display()))?
        .len();

    Ok((context.framework, context.package_manager, output_path, size))
}

fn validate_args(args: &DeployArgs) -> Result<()> {
    for (name, value) in [
        ("git-username", args.git_username.as_str()),
        ("git-repo", args.git_repo.as_str()),
        ("git-hash", args.git_hash.as_str()),
        ("git-branch", args.git_branch.as_str()),
        ("access-token", args.access_token.as_str()),
        ("build-id", args.build_id.as_str()),
    ] {
        if value.trim().is_empty() {
            bail!("--{name} cannot be empty");
        }
    }

    if args.git_username.contains('/') || args.git_username.contains(':') {
        bail!("--git-username contains invalid characters");
    }
    if args.git_repo.contains('/') || args.git_repo.contains(':') {
        bail!("--git-repo must be a repository name, not a URL or owner/repository path");
    }
    if !args.git_hash.chars().all(|c| c.is_ascii_hexdigit()) || args.git_hash.len() < 7 {
        bail!("--git-hash must look like a Git commit SHA");
    }
    if !args.git_branch.chars().all(|c| !c.is_control() && c != '\\') {
        bail!("--git-branch contains invalid characters");
    }

    Ok(())
}

fn clone_repository(args: &DeployArgs, destination: &Path) -> Result<Repository> {
    let url = format!("https://github.com/{}/{}.git", args.git_username, args.git_repo);
    let username = args.git_username.clone();
    let token = args.access_token.clone();

    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(move |_url, username_from_url, allowed_types| {
        if allowed_types.contains(CredentialType::USER_PASS_PLAINTEXT) {
            let login = username_from_url.unwrap_or(&username);
            Cred::userpass_plaintext(login, &token)
        } else {
            Err(git2::Error::from_str("GitHub token authentication is unavailable"))
        }
    });

    let mut fetch_options = FetchOptions::new();
    fetch_options.remote_callbacks(callbacks);

    let mut builder = RepoBuilder::new();
    builder.branch(&args.git_branch);
    builder.fetch_options(fetch_options);

    builder
        .clone(&url, destination)
        .with_context(|| format!("failed to clone GitHub repository {}/{}", args.git_username, args.git_repo))
}

fn checkout_commit(repo_dir: &Path, commit_sha: &str) -> Result<()> {
    let repo = Repository::open(repo_dir).context("failed to open cloned repository")?;
    let oid = Oid::from_str(commit_sha).context("invalid Git commit SHA")?;

    repo.find_commit(oid)
        .with_context(|| format!("commit {commit_sha} was not found in the cloned repository"))?;

    repo.set_head_detached(oid)
        .with_context(|| format!("failed to detach HEAD at commit {commit_sha}"))?;
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .context("failed to checkout requested commit")?;

    Ok(())
}

fn resolve_project_dir(repo_dir: &Path, dir: &str) -> Result<PathBuf> {
    let relative = Path::new(dir);
    if relative.is_absolute() {
        bail!("--dir must be relative to the repository root");
    }
    if relative.components().any(|component| matches!(component, Component::ParentDir)) {
        bail!("--dir cannot contain '..'");
    }

    let project_dir = repo_dir.join(relative);
    let canonical_repo = repo_dir.canonicalize().context("failed to resolve repository path")?;
    let canonical_project = project_dir
        .canonicalize()
        .with_context(|| format!("build directory does not exist: {}", project_dir.display()))?;

    if !canonical_project.starts_with(&canonical_repo) {
        bail!("--dir resolves outside the repository");
    }
    if !canonical_project.is_dir() {
        bail!("--dir is not a directory: {}", canonical_project.display());
    }

    Ok(canonical_project)
}

fn load_package_json(path: &Path) -> Result<Value> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("package.json not found at {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("invalid package.json at {}", path.display()))
}

fn detect_framework(package_json: &Value) -> Result<Framework> {
    if dependency_exists(package_json, "next") {
        return Ok(Framework::NextJs);
    }
    bail!("unsupported framework: package.json does not declare Next.js")
}

fn dependency_exists(package_json: &Value, name: &str) -> bool {
    ["dependencies", "devDependencies", "optionalDependencies"]
        .iter()
        .filter_map(|field| package_json.get(field).and_then(Value::as_object))
        .any(|dependencies| dependencies.contains_key(name))
}

fn detect_package_manager(project_dir: &Path, package_json: &Value) -> Result<PackageManager> {
    if let Some(package_manager) = package_json.get("packageManager").and_then(Value::as_str) {
        if package_manager.starts_with("pnpm@") {
            return Ok(PackageManager::Pnpm);
        }
        if package_manager.starts_with("yarn@") {
            return Ok(PackageManager::Yarn);
        }
        if package_manager.starts_with("npm@") {
            return Ok(PackageManager::Npm);
        }
    }

    if project_dir.join("pnpm-lock.yaml").is_file() {
        return Ok(PackageManager::Pnpm);
    }
    if project_dir.join("yarn.lock").is_file() {
        return Ok(PackageManager::Yarn);
    }
    if project_dir.join("package-lock.json").is_file() {
        return Ok(PackageManager::Npm);
    }

    warn!("no recognized lockfile/packageManager field; falling back to npm install");
    Ok(PackageManager::Npm)
}

fn install_dependencies(context: &BuildContext) -> Result<()> {
    let has_lockfile = context.project_dir.join("package-lock.json").is_file()
        || context.project_dir.join("pnpm-lock.yaml").is_file()
        || context.project_dir.join("yarn.lock").is_file();

    let executable = context.package_manager.executable();
    let args = context.package_manager.install_args(has_lockfile);

    info!(command = %format_command(executable, &args), "installing dependencies");
    run_command(executable, &args, &context.project_dir, "dependency installation")
}

fn run_build(context: &BuildContext) -> Result<()> {
    let scripts = context.package_json.get("scripts").and_then(Value::as_object)
        .context("package.json does not contain a scripts object")?;
    if !scripts.contains_key("build") {
        bail!("package.json does not define a build script");
    }

    let executable = context.package_manager.executable();
    let args = context.package_manager.build_args();

    info!(command = %format_command(executable, &args), "building application");
    run_command(executable, &args, &context.project_dir, "application build")
}

fn run_command(executable: &str, args: &[&str], cwd: &Path, operation: &str) -> Result<()> {
    let status = Command::new(executable)
        .args(args)
        .current_dir(cwd)
        .env("CI", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to start {operation}: {}", format_command(executable, args)))?;

    if !status.success() {
        bail!(
            "{operation} failed with exit status {}",
            status.code().map_or_else(|| "terminated by signal".to_owned(), |code| code.to_string())
        );
    }
    Ok(())
}

fn format_command(executable: &str, args: &[&str]) -> String {
    std::iter::once(executable)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

fn artifact_output_path(build_id: &str) -> Result<PathBuf> {
    let safe_id: String = build_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();

    if safe_id.is_empty() {
        bail!("--build-id produced an empty artifact name");
    }

    Ok(std::env::current_dir()?.join(format!("artifact-{safe_id}.zip")))
}

fn create_artifact(context: &BuildContext, next_dir: &Path, output_path: &Path) -> Result<()> {
    let file = File::create(output_path)
        .with_context(|| format!("failed to create artifact {}", output_path.display()))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .large_file(true);

    add_directory_to_zip(&mut zip, next_dir, next_dir, options)?;
    add_file_to_zip(
        &mut zip,
        &context.project_dir.join("package.json"),
        Path::new("package.json"),
        options,
    )?;

    zip.finish().context("failed to finalize artifact ZIP")?;
    Ok(())
}

fn add_directory_to_zip(
    zip: &mut ZipWriter<File>,
    root: &Path,
    current: &Path,
    options: SimpleFileOptions,
) -> Result<()> {
    for entry in WalkDir::new(current).sort_by_file_name() {
        let entry = entry.context("failed while walking .next directory")?;
        let path = entry.path();

        if path.is_dir() {
            let relative = path.strip_prefix(root).context("failed to calculate artifact directory path")?;
            let archive_path = if relative.as_os_str().is_empty() {
                PathBuf::from(".next/")
            } else {
                PathBuf::from(".next").join(relative)
            };
            zip.add_directory(path_to_zip_name(&archive_path), options)
                .with_context(|| format!("failed to add directory {}", path.display()))?;
        } else if path.is_file() {
            let relative = path.strip_prefix(root).context("failed to calculate artifact file path")?;
            add_file_to_zip(zip, path, &PathBuf::from(".next").join(relative), options)?;
        }
    }
    Ok(())
}

fn add_file_to_zip(
    zip: &mut ZipWriter<File>,
    source: &Path,
    archive_path: &Path,
    options: SimpleFileOptions,
) -> Result<()> {
    zip.start_file(path_to_zip_name(archive_path), options)
        .with_context(|| format!("failed to create ZIP entry {}", archive_path.display()))?;

    let mut input = File::open(source)
        .with_context(|| format!("failed to open artifact source {}", source.display()))?;
    io::copy(&mut input, zip)
        .with_context(|| format!("failed to copy {} into artifact", source.display()))?;

    Ok(())
}

fn path_to_zip_name(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str(),
            Component::CurDir => Some("."),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}
