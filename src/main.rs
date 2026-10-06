use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use git2::{build::RepoBuilder, Cred, CredentialType, FetchOptions, Oid, RemoteCallbacks, Repository};
use serde::Serialize;
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{self, Read, Write},
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

    let standalone_dir = prepare_standalone_output(&context)?;
    let output_path = artifact_output_path(&args.build_id)?;
    create_artifact(&standalone_dir, &output_path)?;

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
    callbacks.credentials(move |_url, _username_from_url, allowed_types| {
        if allowed_types.contains(CredentialType::USER_PASS_PLAINTEXT) {
            // GitHub accepts a PAT as the password for HTTPS authentication.
            // Use the conventional x-access-token username instead of depending
            // on the username returned by the remote.
            Cred::userpass_plaintext("x-access-token", &token)
        } else {
            Err(git2::Error::from_str(
                "GitHub HTTPS authentication requires USER_PASS_PLAINTEXT credentials",
            ))
        }
    });

    let mut fetch_options = FetchOptions::new();
    fetch_options.remote_callbacks(callbacks);

    let mut builder = RepoBuilder::new();
    builder.branch(&args.git_branch);
    builder.fetch_options(fetch_options);

    builder.clone(&url, destination).map_err(|error| {
        anyhow::anyhow!(
            "failed to clone GitHub repository {}/{}: {} (class: {:?}, code: {:?})",
            args.git_username,
            args.git_repo,
            error.message(),
            error.class(),
            error.code()
        )
    })
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

    let config_override = NextConfigOverride::apply(&context.project_dir, &context.package_json)
        .context("failed to configure Next.js standalone output")?;

    let executable = context.package_manager.executable();
    let args = context.package_manager.build_args();

    info!(
        command = %format_command(executable, &args),
        "building application with standalone output enabled"
    );

    let build_result = run_command(executable, &args, &context.project_dir, "application build");
    config_override.restore()?;
    build_result
}

struct NextConfigOverride {
    config_path: PathBuf,
    backup_path: Option<PathBuf>,
}

impl NextConfigOverride {
    fn apply(project_dir: &Path, package_json: &Value) -> Result<Self> {
        let candidates = [
            "next.config.js",
            "next.config.mjs",
            "next.config.cjs",
            "next.config.ts",
        ];

        let config_name = candidates
            .iter()
            .find(|candidate| project_dir.join(candidate).is_file())
            .copied();

        let (config_path, backup_path, source_name) = match config_name {
            Some(name) => {
                let config_path = project_dir.join(name);
                let backup_path = project_dir.join(format!(".meshscale-original-{name}"));

                if backup_path.exists() {
                    bail!(
                        "temporary Next.js config backup already exists: {}",
                        backup_path.display()
                    );
                }

                fs::rename(&config_path, &backup_path).with_context(|| {
                    format!(
                        "failed to temporarily move existing Next.js config {}",
                        config_path.display()
                    )
                })?;

                (config_path, Some(backup_path), name.to_owned())
            }
            None => (project_dir.join("next.config.js"), None, "next.config.js".to_owned()),
        };

        let is_esm = match source_name.as_str() {
            "next.config.mjs" => true,
            "next.config.cjs" => false,
            "next.config.ts" => true,
            "next.config.js" => package_json
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|value| value == "module"),
            _ => false,
        };

        let original_import = backup_path
            .as_ref()
            .map(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .expect("backup filename is valid UTF-8")
                    .to_owned()
            });

        let wrapper = if let Some(original_name) = original_import {
            standalone_config_wrapper(&original_name, is_esm)
        } else {
            standalone_config_wrapper_for_new_file(is_esm)
        };

        if let Err(error) = fs::write(&config_path, wrapper) {
            if let Some(backup_path) = &backup_path {
                let _ = fs::remove_file(&config_path);
                let _ = fs::rename(backup_path, &config_path);
            }
            return Err(error).with_context(|| {
                format!("failed to write temporary Next.js config {}", config_path.display())
            });
        }

        Ok(Self {
            config_path,
            backup_path,
        })
    }

    fn restore(mut self) -> Result<()> {
        self.restore_inner()
    }

    fn restore_inner(&mut self) -> Result<()> {
        if !self.config_path.exists() {
            if let Some(backup_path) = &self.backup_path {
                fs::rename(backup_path, &self.config_path).with_context(|| {
                    format!(
                        "failed to restore original Next.js config {}",
                        self.config_path.display()
                    )
                })?;
            }
        } else {
            fs::remove_file(&self.config_path).with_context(|| {
                format!(
                    "failed to remove temporary Next.js config {}",
                    self.config_path.display()
                )
            })?;

            if let Some(backup_path) = &self.backup_path {
                fs::rename(backup_path, &self.config_path).with_context(|| {
                    format!(
                        "failed to restore original Next.js config {}",
                        self.config_path.display()
                    )
                })?;
            }
        }

        self.backup_path = None;
        Ok(())
    }
}

impl Drop for NextConfigOverride {
    fn drop(&mut self) {
        if self.backup_path.is_some() {
            if let Err(error) = self.restore_inner() {
                warn!(error = %error, "failed to restore original Next.js config during cleanup");
            }
        }
    }
}

fn standalone_config_wrapper(original_name: &str, is_esm: bool) -> String {
    let import_name = original_name
        .strip_suffix(".ts")
        .unwrap_or(original_name);

    if is_esm {
        format!(
            "import original from './{import_name}';

const forceStandalone = (config) => ({{ ...(config || {{}}), output: 'standalone' }});

export default typeof original === 'function'
  ? (...args) => {{
      const result = original(...args);
      return result && typeof result.then === 'function'
        ? result.then(forceStandalone)
        : forceStandalone(result);
    }}
  : forceStandalone(original);
"
        )
    } else {
        format!(
            "const original = require('./{original_name}');

const forceStandalone = (config) => ({{ ...(config || {{}}), output: 'standalone' }});

module.exports = typeof original === 'function'
  ? (...args) => {{
      const result = original(...args);
      return result && typeof result.then === 'function'
        ? result.then(forceStandalone)
        : forceStandalone(result);
    }}
  : forceStandalone(original);
"
        )
    }
}

fn standalone_config_wrapper_for_new_file(is_esm: bool) -> String {
    if is_esm {
        "export default { output: 'standalone' };\n".to_owned()
    } else {
        "module.exports = { output: 'standalone' };\n".to_owned()
    }
}

fn prepare_standalone_output(context: &BuildContext) -> Result<PathBuf> {
    let standalone_dir = context.project_dir.join(".next").join("standalone");
    if !standalone_dir.is_dir() {
        bail!(
            "build completed but Next.js standalone directory was not produced: {}",
            standalone_dir.display()
        );
    }

    info!("preparing self-contained Next.js standalone output");

    let public_dir = context.project_dir.join("public");
    if public_dir.is_dir() {
        copy_directory_contents(&public_dir, &standalone_dir.join("public"))
            .context("failed to copy public assets into standalone output")?;
    }

    let static_dir = context.project_dir.join(".next").join("static");
    if static_dir.is_dir() {
        copy_directory_contents(
            &static_dir,
            &standalone_dir.join(".next").join("static"),
        )
        .context("failed to copy Next.js static assets into standalone output")?;
    }

    Ok(standalone_dir)
}

fn copy_directory_contents(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;

    for entry in WalkDir::new(source).sort_by_file_name() {
        let entry = entry.context("failed while walking directory to copy")?;
        let source_path = entry.path();
        let relative = source_path
            .strip_prefix(source)
            .context("failed to calculate copied file path")?;
        let destination_path = destination.join(relative);

        if source_path.is_dir() {
            fs::create_dir_all(&destination_path)
                .with_context(|| format!("failed to create {}", destination_path.display()))?;
        } else if source_path.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            fs::copy(source_path, &destination_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
        }
    }

    Ok(())
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

struct ZipProgress {
    total_files: u64,
    total_bytes: u64,
    files_done: u64,
    bytes_done: u64,
    last_percent: u8,
}

fn create_artifact(standalone_dir: &Path, output_path: &Path) -> Result<()> {
    let (total_files, total_bytes) = collect_artifact_stats(standalone_dir)?;
    info!(
        files = total_files,
        bytes = total_bytes,
        "starting artifact packaging"
    );

    let temporary_output = output_path.with_extension("zip.tmp");
    let file = File::create(&temporary_output).with_context(|| {
        format!(
            "failed to create temporary artifact {}",
            temporary_output.display()
        )
    })?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .large_file(true);
    let mut progress = ZipProgress {
        total_files,
        total_bytes,
        files_done: 0,
        bytes_done: 0,
        last_percent: 0,
    };

    progress.log();

    for entry in WalkDir::new(standalone_dir).sort_by_file_name() {
        let entry = entry.context("failed while walking standalone directory")?;
        let path = entry.path();

        if path.is_dir() {
            let relative = path
                .strip_prefix(standalone_dir)
                .context("failed to calculate artifact directory path")?;

            if !relative.as_os_str().is_empty() {
                zip.add_directory(path_to_zip_name(relative), options)
                    .with_context(|| format!("failed to add directory {}", path.display()))?;
            }
        } else if path.is_file() {
            let relative = path
                .strip_prefix(standalone_dir)
                .context("failed to calculate artifact file path")?;
            add_file_to_zip(
                &mut zip,
                path,
                relative,
                options,
                &mut progress,
            )?;
        }
    }

    zip.finish().context("failed to finalize artifact ZIP")?;
    fs::rename(&temporary_output, output_path).with_context(|| {
        format!(
            "failed to move completed artifact into place: {}",
            output_path.display()
        )
    })?;

    info!(
        files = progress.files_done,
        source_bytes = progress.bytes_done,
        "artifact packaging complete"
    );
    Ok(())
}

fn collect_artifact_stats(root: &Path) -> Result<(u64, u64)> {
    let mut files = 0;
    let mut bytes = 0;

    for entry in WalkDir::new(root).sort_by_file_name() {
        let entry = entry.context("failed while scanning standalone output")?;
        if entry.path().is_file() {
            files += 1;
            bytes += entry
                .metadata()
                .with_context(|| format!("failed to stat {}", entry.path().display()))?
                .len();
        }
    }

    Ok((files, bytes))
}

fn add_file_to_zip(
    zip: &mut ZipWriter<File>,
    source: &Path,
    archive_path: &Path,
    options: SimpleFileOptions,
    progress: &mut ZipProgress,
) -> Result<()> {
    zip.start_file(path_to_zip_name(archive_path), options)
        .with_context(|| format!("failed to create ZIP entry {}", archive_path.display()))?;

    let mut input = File::open(source)
        .with_context(|| format!("failed to open artifact source {}", source.display()))?;
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = input
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", source.display()))?;
        if read == 0 {
            break;
        }

        zip.write_all(&buffer[..read])
            .with_context(|| format!("failed to write {} into artifact", source.display()))?;
        progress.bytes_done += read as u64;
        progress.log();
    }

    progress.files_done += 1;
    progress.log();
    Ok(())
}

impl ZipProgress {
    fn log(&mut self) {
        let percent = if self.total_bytes == 0 {
            if self.total_files == 0 {
                100
            } else {
                ((self.files_done * 100) / self.total_files).min(100) as u8
            }
        } else {
            ((self.bytes_done.saturating_mul(100)) / self.total_bytes).min(100) as u8
        };

        if percent != self.last_percent || percent == 100 {
            self.last_percent = percent;
            info!(
                progress = %format!("{percent}%"),
                files = %format!("{}/{}", self.files_done, self.total_files),
                source_bytes = %format!("{}/{}", format_bytes(self.bytes_done), format_bytes(self.total_bytes)),
                "zipping standalone output"
            );
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;

    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
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
