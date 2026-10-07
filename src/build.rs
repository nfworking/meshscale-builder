use anyhow::{Context, Result, bail};
use git2::{
    Cred, CredentialType, FetchOptions, Oid, RemoteCallbacks, Repository, build::RepoBuilder,
};
use serde_json::Value;
use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::TempDir;
use tracing::{info, warn};

use crate::artifact::create_output;
use crate::{BuildArgs, BuildMetadata, Framework, PackageManager};

pub fn build_application(args: &BuildArgs) -> Result<(BuildMetadata, PathBuf)> {
    validate_args(args)?;

    let workspace = TempDir::new().context("failed to create temporary build workspace")?;
    let repo_dir = workspace.path().join("repo");

    info!(
        build_id = %args.build_id,
        repository = %format!("{}/{}", args.git_username, args.git_repo),
        "cloning repository"
    );

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

    install_dependencies(&project_dir, package_manager)?;
    install_adapter_runtime_dependency(&project_dir)?;
    run_build(&project_dir, package_manager, &package_json)?;

    let metadata = BuildMetadata {
        version: 2,
        framework,
        package_manager,
        build_id: args.build_id.clone(),
        repository: format!("{}/{}", args.git_username, args.git_repo),
        commit: Repository::open(&repo_dir)?
            .head()?
            .peel_to_commit()?
            .id()
            .to_string(),
        branch: args.git_branch.clone(),
        org_id: args.destination.org_id.clone(),
        project_id: args.destination.project_id.clone(),
    };

    let output_dir = create_output(&project_dir, &metadata, &args.output)?;
    Ok((metadata, output_dir))
}

fn validate_args(args: &BuildArgs) -> Result<()> {
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
    if !args
        .git_branch
        .chars()
        .all(|c| !c.is_control() && c != '\\')
    {
        bail!("--git-branch contains invalid characters");
    }

    if args.output.as_os_str().is_empty() {
        bail!("--output cannot be empty");
    }

    Ok(())
}

fn clone_repository(args: &BuildArgs, destination: &Path) -> Result<Repository> {
    let url = format!(
        "https://github.com/{}/{}.git",
        args.git_username, args.git_repo
    );
    let token = args.access_token.clone();

    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(move |_url, _username_from_url, allowed_types| {
        if allowed_types.contains(CredentialType::USER_PASS_PLAINTEXT) {
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
    if relative
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("--dir cannot contain '..'");
    }

    let project_dir = repo_dir.join(relative);
    let canonical_repo = repo_dir
        .canonicalize()
        .context("failed to resolve repository path")?;
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
    serde_json::from_str(&content)
        .with_context(|| format!("invalid package.json at {}", path.display()))
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

fn install_dependencies(project_dir: &Path, package_manager: PackageManager) -> Result<()> {
    let has_lockfile = project_dir.join("package-lock.json").is_file()
        || project_dir.join("pnpm-lock.yaml").is_file()
        || project_dir.join("yarn.lock").is_file();

    let executable = package_manager.executable();
    let args = package_manager.install_args(has_lockfile);

    info!(command = %format_command(executable, &args), "installing dependencies");
    run_command(executable, &args, project_dir, "dependency installation")
}

fn install_adapter_runtime_dependency(project_dir: &Path) -> Result<()> {
    let next_package = project_dir.join("node_modules").join("next").join("package.json");
    let package: Value = serde_json::from_slice(
        &fs::read(&next_package).with_context(|| format!("failed to read {}", next_package.display()))?,
    )
    .context("installed Next.js package.json is invalid")?;
    let version = package
        .get("version")
        .and_then(Value::as_str)
        .context("installed Next.js package.json is missing version")?;

    let spec = format!("@next/routing@{version}");
    let args = ["install", "--no-save", "--package-lock=false", "--ignore-scripts", spec.as_str()];
    info!(command = %format_command("npm", &args), "installing Next.js adapter routing runtime");
    run_command("npm", &args, project_dir, "adapter routing installation")
}

fn run_build(
    project_dir: &Path,
    package_manager: PackageManager,
    package_json: &Value,
) -> Result<()> {
    let scripts = package_json
        .get("scripts")
        .and_then(Value::as_object)
        .context("package.json does not contain a scripts object")?;

    if !scripts.contains_key("build") {
        bail!("package.json does not define a build script");
    }

    let executable = package_manager.executable();
    let args = package_manager.build_args();

    let adapter_path = project_dir.join(".meshscale-next-adapter.cjs");
    fs::write(&adapter_path, include_str!("next_adapter.cjs"))
        .context("failed to stage MeshScale Next.js adapter")?;

    info!(
        command = %format_command(executable, &args),
        adapter = %adapter_path.display(),
        "building application through the MeshScale Next.js adapter"
    );

    let mut command = Command::new(executable);
    crate::upload::remove_credentials(&mut command);
    let status = command
        .args(&args)
        .current_dir(project_dir)
        .env("CI", "true")
        .env("NEXT_ADAPTER_PATH", &adapter_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| {
            format!(
                "failed to start application build: {}",
                format_command(executable, &args)
            )
        });

    let _ = fs::remove_file(&adapter_path);
    let status = status?;
    if !status.success() {
        bail!(
            "application build failed with exit status {}",
            status.code().map_or_else(
                || "terminated by signal".to_owned(),
                |code| code.to_string()
            )
        );
    }

    Ok(())
}

fn run_command(executable: &str, args: &[&str], cwd: &Path, operation: &str) -> Result<()> {
    let mut command = Command::new(executable);
    crate::upload::remove_credentials(&mut command);
    let status = command
        .args(args)
        .current_dir(cwd)
        .env("CI", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| {
            format!(
                "failed to start {operation}: {}",
                format_command(executable, args)
            )
        })?;

    if !status.success() {
        bail!(
            "{operation} failed with exit status {}",
            status.code().map_or_else(
                || "terminated by signal".to_owned(),
                |code| code.to_string()
            )
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
