use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod artifact;
mod build;
mod cache;
mod cli;
mod env;
mod lambda;
mod manifest;
mod routing;
mod runner;
mod static_output;
mod stats;
mod upload;

#[derive(Parser, Debug)]
#[command(
    name = "meshscale-builder",
    version,
    about = "MeshScale internal application builder"
)]
struct Cli {
    /// Load R2 defaults from this dotenv file (otherwise search current directory and parents).
    #[arg(long, global = true)]
    env_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Build a deployment artifact and upload it to R2 unless --no-upload is set.
    Build(BuildArgs),
    /// Serve static assets and proxy dynamic requests to a managed Node runtime.
    Run(runner::RunArgs),
    /// Upload a previously built output to an immutable R2 deployment prefix.
    Upload(upload::UploadArgs),
    /// Package a previously built output for a deployment target.
    Package(lambda::PackageArgs),
}

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// Directory containing package.json, relative to the repository root.
    #[arg(long, default_value = ".")]
    pub dir: String,
    /// GitHub owner/username.
    #[arg(long = "git-username")]
    pub git_username: String,
    /// Repository name.
    #[arg(long = "git-repo")]
    pub git_repo: String,
    /// Exact commit SHA to build.
    #[arg(long = "git-hash")]
    pub git_hash: String,
    /// Branch to clone.
    #[arg(long = "git-branch")]
    pub git_branch: String,
    /// Optional GitHub token for private repository access.
    /// Prefer MESHSCALE_GITHUB_TOKEN so the credential is not exposed in process arguments.
    #[arg(long = "access-token", value_name = "TOKEN")]
    pub access_token: Option<String>,
    /// MeshScale build identifier.
    #[arg(long = "build-id")]
    pub build_id: String,
    /// Destination for the build output.
    #[arg(long, default_value = ".meshscale/output")]
    pub output: PathBuf,
    #[command(flatten)]
    pub destination: upload::DestinationArgs,
    /// Build locally without R2 credentials or uploading.
    #[arg(long)]
    pub no_upload: bool,
    /// Dotenv file with variables for install and build commands (for example NEXT_PUBLIC_*
    /// or npm_config_*). The builder's own environment is not inherited except for a small
    /// allowlist (PATH, HOME, locale, temp, proxy and CA settings).
    #[arg(long, value_name = "PATH")]
    pub build_env_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Framework {
    NextJs,
}

impl Framework {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NextJs => "nextjs",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    Npm,
    Pnpm,
    Yarn,
}

impl PackageManager {
    pub fn executable(self) -> &'static str {
        match self {
            Self::Npm => if cfg!(windows) { "npm.cmd" } else { "npm" },
            Self::Pnpm => if cfg!(windows) { "pnpm.cmd" } else { "pnpm" },
            Self::Yarn => if cfg!(windows) { "yarn.cmd" } else { "yarn" },
        }
    }

    pub fn install_args(self, has_lockfile: bool) -> Vec<&'static str> {
        match (self, has_lockfile) {
            (Self::Npm, true) => vec!["ci"],
            (Self::Npm, false) => vec!["install"],
            (Self::Pnpm, true) => vec!["install", "--frozen-lockfile"],
            (Self::Pnpm, false) => vec!["install"],
            (Self::Yarn, true) => vec!["install", "--frozen-lockfile"],
            (Self::Yarn, false) => vec!["install"],
        }
    }

    pub fn build_args(self) -> Vec<&'static str> {
        match self {
            Self::Npm | Self::Pnpm => vec!["run", "build"],
            Self::Yarn => vec!["build"],
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildMetadata {
    pub version: u8,
    pub framework: Framework,
    pub package_manager: PackageManager,
    pub build_id: String,
    pub repository: String,
    pub commit: String,
    pub branch: String,
    pub org_id: Option<String>,
    pub project_id: Option<String>,
}

impl BuildMetadata {
    pub fn next_version(&self) -> String {
        "installed".to_owned()
    }
}

struct FileLogGuard {
    _guard: tracing_appender::non_blocking::WorkerGuard,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Build(args) => run_build(args, cli.env_file.as_deref()),
        Commands::Run(args) => {
            init_terminal_logging();
            runner::run(args)
        }
        Commands::Upload(args) => {
            init_terminal_logging();
            upload::upload(args, cli.env_file.as_deref())
        }
        Commands::Package(args) => {
            init_terminal_logging();
            lambda::package(args)
        }
    };

    if let Err(error) = result {
        eprintln!("builder failed: {error:#}");
        std::process::exit(1);
    }
}

fn init_terminal_logging() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "meshscale_builder=info".into()),
        )
        .with_target(false)
        .try_init();
}

fn init_build_logging(output: &Path) -> Result<FileLogGuard> {
    fs::create_dir_all(output)?;
    let log_path = output.join("build.log");
    let _ = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&log_path)?;
    let appender = tracing_appender::rolling::never(output, "build.log");
    let (file_writer, guard) = tracing_appender::non_blocking(appender);

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| "meshscale_builder=info".into());

    // Build tracing is intentionally file-only. The terminal is owned exclusively
    // by the custom BuildProgress UI below; internal stage logs must never leak into it.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file_writer)
                .with_target(true)
                .with_ansi(false)
                .with_thread_ids(true)
                .with_thread_names(true),
        )
        .with(env_filter)
        .try_init()?;

    Ok(FileLogGuard { _guard: guard })
}

fn run_build(args: BuildArgs, env_file: Option<&std::path::Path>) -> Result<()> {
    let started = Instant::now();
    let build_id = args.build_id.clone();
    let output_path = if args.output.is_absolute() {
        args.output.clone()
    } else {
        std::env::current_dir()?.join(&args.output)
    };
    let _log_guard = init_build_logging(&output_path)?;

    tracing::info!(
        build_id = %args.build_id,
        project_id = ?args.destination.project_id,
        repository = %format!("{}/{}", args.git_username, args.git_repo),
        commit = %args.git_hash,
        branch = %args.git_branch,
        "build started"
    );

    let progress = cli::BuildProgress::new();

    println!();
    println!("MeshScale Build");
    println!("───────────────");
    println!("  build     {}", args.build_id);
    println!(
        "  project   {}",
        args.destination.project_id.as_deref().unwrap_or("—")
    );
    println!();

    let build = (|| {
        progress.step("Validating build configuration", || {
            upload::validate_id("build-id", &args.build_id)?;
            if args.no_upload {
                args.destination.validate_optional()?;
            } else {
                args.destination.validate()?;
            }
            Ok(())
        })?;

        let config = if args.no_upload {
            None
        } else {
            Some(progress.step("Loading upload configuration", || {
                upload::R2Config::load(env_file)
            })?)
        };

        let (metadata, output_path) =
            build::build_application(&args, &progress)?;

        let uploaded = if let Some(config) = config {
            Some(progress.step("Uploading deployment artifact", || {
                upload::upload_output(
                    &output_path,
                    &args.destination,
                    Some(&args.build_id),
                    config,
                )
            })?)
        } else {
            None
        };

        Ok::<_, anyhow::Error>((metadata, output_path, uploaded))
    })();

    match build {
        Ok((metadata, output_path, uploaded)) => {
            let elapsed = started.elapsed();
            let static_size = cli::output_size(&output_path.join("static"))?;
            let runtime_size = cli::output_size(&output_path.join("runtime"))?;

            tracing::info!(
                build_id = %metadata.build_id,
                project_id = ?metadata.project_id,
                static_bytes = static_size,
                runtime_bytes = runtime_size,
                build_time_ms = elapsed.as_millis(),
                "build completed"
            );

            println!();
            println!("✓ Build completed");
            println!();
            println!("  Build ID        {}", metadata.build_id);
            println!(
                "  Project ID      {}",
                metadata.project_id.as_deref().unwrap_or("—")
            );
            println!("  Framework       {}", metadata.framework.as_str());
            println!("  Package manager {}", metadata.package_manager.as_str());
            println!("  Static size     {}", cli::format_bytes(static_size));
            println!("  Runtime size    {}", cli::format_bytes(runtime_size));
            println!("  Build time      {}", cli::format_duration(elapsed));
            println!("  Output          {}", output_path.display());
            if uploaded.is_some() {
                println!("  Upload          complete");
            } else {
                println!("  Upload          skipped");
            }
            println!();

            // Make the final record immediately useful when the command is piped,
            // while keeping the normal terminal output human-first.
            io::stdout().flush()?;

            Ok(())
        }
        Err(error) => {
            let elapsed = started.elapsed();
            tracing::error!(
                build_id = %build_id,
                elapsed_ms = elapsed.as_millis(),
                error = %format!("{error:#}"),
                "build failed"
            );
            eprintln!();
            eprintln!("✗ Build failed");
            eprintln!("  {error:#}");
            eprintln!();
            eprintln!("  Detailed log: {}", output_path.join("build.log").display());
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_upload_and_default_build_upload_with_local_opt_out() {
        let cli = Cli::try_parse_from([
            "meshscale-builder",
            "--env-file",
            "r2.env",
            "upload",
            ".meshscale/output",
            "--org-id",
            "org_1",
            "--project-id",
            "project_1",
            "--build-id",
            "build_1",
        ])
        .unwrap();
        assert_eq!(cli.env_file, Some(PathBuf::from("r2.env")));
        let Commands::Upload(args) = cli.command else {
            panic!("expected upload");
        };
        assert_eq!(args.destination.validate().unwrap(), ("org_1", "project_1"));
        assert_eq!(args.build_id.as_deref(), Some("build_1"));
        for opt_out in [false, true] {
            let mut cli = vec![
                "meshscale-builder",
                "build",
                "--git-username",
                "owner",
                "--git-repo",
                "repo",
                "--git-hash",
                "1234567",
                "--git-branch",
                "main",
                "--access-token",
                "test",
                "--build-id",
                "build_1",
            ];
            if opt_out {
                cli.push("--no-upload");
            } else {
                cli.extend(["--org-id", "org_1", "--project-id", "project_1"]);
            }
            let Commands::Build(args) = Cli::try_parse_from(cli).unwrap().command else {
                panic!("expected build");
            };
            assert_eq!(args.no_upload, opt_out);
            assert_eq!(args.destination.validate().is_ok(), !opt_out);
            args.destination.validate_optional().unwrap();
            assert!(args.build_env_file.is_none());
        }
        let Commands::Build(args) = Cli::try_parse_from([
            "meshscale-builder",
            "build",
            "--git-username",
            "owner",
            "--git-repo",
            "repo",
            "--git-hash",
            "1234567",
            "--git-branch",
            "main",
            "--build-id",
            "build_1",
            "--no-upload",
            "--build-env-file",
            "project.env",
        ])
        .unwrap()
        .command
        else {
            panic!("expected build");
        };
        assert_eq!(args.build_env_file, Some(PathBuf::from("project.env")));
        assert!(
            Cli::try_parse_from(["meshscale-builder", "upload", "output", "--org-id", "org"])
                .is_err()
        );
    }

    #[test]
    fn parses_run_defaults_and_projects() {
        let cli = Cli::try_parse_from([
            "meshscale-builder",
            "run",
            ".meshscale/output",
            "--port",
            "8080",
        ])
        .unwrap();
        let Commands::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.output, Some(PathBuf::from(".meshscale/output")));
        assert_eq!(args.port, 8080);
        assert!(args.projects.is_empty());
        assert!(!args.lambda_local);

        let Commands::Run(args) = Cli::try_parse_from([
            "meshscale-builder",
            "run",
            ".meshscale/output",
            "--lambda-local",
            "--idle-timeout-secs",
            "5",
        ])
        .unwrap()
        .command
        else {
            panic!("expected run command");
        };
        assert!(args.lambda_local);
        assert_eq!(args.idle_timeout_secs, 5);

        let cli = Cli::try_parse_from([
            "meshscale-builder",
            "run",
            "--project",
            "app.localhost=./app-output",
            "--project",
            "blog.localhost=./blog-output",
        ])
        .unwrap();
        let Commands::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert!(args.output.is_none());
        assert_eq!(args.projects.len(), 2);

        assert!(
            Cli::try_parse_from([
                "meshscale-builder",
                "run",
                ".meshscale/output",
                "--port",
                "0"
            ])
            .is_err()
        );
    }
}
