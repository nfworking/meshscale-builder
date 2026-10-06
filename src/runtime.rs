use anyhow::{bail, Result};
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Path to a previously built .meshscale/output directory.
    pub output: PathBuf,
}

pub fn run(_args: RunArgs) -> Result<()> {
    bail!("phase D (local runtime) is intentionally not implemented yet")
}
