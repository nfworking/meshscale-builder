use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use std::{path::PathBuf, time::Instant};
use tracing_subscriber::EnvFilter;

mod artifact;
mod build;
mod runtime;
mod upload;

#[derive(Parser, Debug)]
#[command(name = "meshscale-builder", version, about = "MeshScale internal application builder")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Clone, detect, install, build and produce .meshscale/output.
    Build(BuildArgs),
    /// Run a previously built output locally (phase D; not implemented yet).
    Run(runtime::RunArgs),
    /// Upload a previously built output to R2 (phase E; not implemented yet).
    Upload(upload::UploadArgs),
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
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Framework { NextJs }

impl Framework {
    pub fn as_str(self) -> &'static str {
        match self { Self::NextJs => "nextjs" }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager { Npm, Pnpm, Yarn }

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
        match self { Self::Npm => "npm", Self::Pnpm => "pnpm", Self::Yarn => "yarn" }
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
}

impl BuildMetadata {
    pub fn next_version(&self) -> String { "installed".to_owned() }
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
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "meshscale_builder=info".into()))
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Build(args) => run_build(args),
        Commands::Run(args) => runtime::run(args),
        Commands::Upload(args) => upload::upload(args),
    };

    if let Err(error) = result {
        eprintln!("builder failed: {error:#}");
        std::process::exit(1);
    }
}

fn run_build(args: BuildArgs) -> Result<()> {
    let started = Instant::now();
    let build_id = args.build_id.clone();

    match build::build_application(&args) {
        Ok((metadata, output_path)) => {
            let result = BuildResult {
                status: "success",
                build_id,
                framework: Some(metadata.framework.as_str()),
                package_manager: Some(metadata.package_manager.as_str()),
                output_path: Some(output_path.display().to_string()),
                build_time_ms: started.elapsed().as_millis(),
                error: None,
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
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
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
            std::process::exit(1);
        }
    }
}