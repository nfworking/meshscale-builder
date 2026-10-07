use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use std::{path::PathBuf, time::Instant};
use tracing_subscriber::EnvFilter;

mod archive;
mod artifact;
mod build;
mod cache;
mod manifest;
mod routing;
mod runtime;
mod static_output;
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
    Run(runtime::RunArgs),
    /// Upload a previously built output to an immutable R2 deployment prefix.
    Upload(upload::UploadArgs),
    /// Create a local server ZIP without uploading or requiring R2 credentials.
    Package(archive::PackageArgs),
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
    /// GitHub token used for repository authentication.
    #[arg(long = "access-token")]
    pub access_token: String,
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
            Self::Npm => {
                if cfg!(windows) {
                    "npm.cmd"
                } else {
                    "npm"
                }
            }
            Self::Pnpm => {
                if cfg!(windows) {
                    "pnpm.cmd"
                } else {
                    "pnpm"
                }
            }
            Self::Yarn => {
                if cfg!(windows) {
                    "yarn.cmd"
                } else {
                    "yarn"
                }
            }
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

#[derive(Debug, Serialize)]
struct BuildResult {
    status: &'static str,
    build_id: String,
    framework: Option<&'static str>,
    package_manager: Option<&'static str>,
    output_path: Option<String>,
    build_time_ms: u128,
    error: Option<String>,
    upload: Option<upload::UploadResult>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "meshscale_builder=info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Build(args) => run_build(args, cli.env_file.as_deref()),
        Commands::Run(args) => runtime::run(args),
        Commands::Upload(args) => upload::upload(args, cli.env_file.as_deref()),
        Commands::Package(args) => archive::package(args),
    };

    if let Err(error) = result {
        eprintln!("builder failed: {error:#}");
        std::process::exit(1);
    }
}

fn run_build(args: BuildArgs, env_file: Option<&std::path::Path>) -> Result<()> {
    let started = Instant::now();
    let build_id = args.build_id.clone();

    let build = (|| {
        upload::validate_id("build-id", &args.build_id)?;
        let config = if args.no_upload {
            args.destination.validate_optional()?;
            None
        } else {
            args.destination.validate()?;
            Some(upload::R2Config::load(env_file)?)
        };
        let (metadata, output_path) = build::build_application(&args)?;
        let uploaded = config
            .map(|config| {
                upload::upload_output(
                    &output_path,
                    &args.destination,
                    Some(&args.build_id),
                    config,
                )
            })
            .transpose();
        Ok::<_, anyhow::Error>((metadata, output_path, uploaded))
    })();
    match build {
        Ok((metadata, output_path, uploaded)) => {
            let error = uploaded.as_ref().err().map(|error| format!("{error:#}"));
            let succeeded = error.is_none();
            let result = BuildResult {
                status: if succeeded { "success" } else { "failed" },
                build_id,
                framework: Some(metadata.framework.as_str()),
                package_manager: Some(metadata.package_manager.as_str()),
                output_path: Some(output_path.display().to_string()),
                build_time_ms: started.elapsed().as_millis(),
                error,
                upload: uploaded.ok().flatten(),
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
            if !succeeded {
                anyhow::bail!(
                    "build succeeded but upload failed; local artifact remains at {}",
                    output_path.display()
                );
            }
            Ok(())
        }
        Err(error) => {
            let result = BuildResult {
                status: "failed",
                build_id,
                framework: None,
                package_manager: None,
                output_path: None,
                build_time_ms: started.elapsed().as_millis(),
                error: Some(error.to_string()),
                upload: None,
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
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
        }
        assert!(
            Cli::try_parse_from(["meshscale-builder", "upload", "output", "--org-id", "org"])
                .is_err()
        );
    }

    #[test]
    fn parses_run_defaults_and_custom_ports() {
        for (extra, expected) in [
            (vec![], (3000, 3100)),
            (
                vec!["--port", "8080", "--runtime-port", "8100"],
                (8080, 8100),
            ),
        ] {
            let mut args = vec!["meshscale-builder", "run", ".meshscale/output"];
            args.extend(extra);
            let cli = Cli::try_parse_from(args).unwrap();
            let Commands::Run(args) = cli.command else {
                panic!("expected run command");
            };
            assert_eq!((args.port, args.runtime_port), expected);
        }
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
