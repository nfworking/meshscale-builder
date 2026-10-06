use anyhow::{bail, Result};
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct UploadArgs {
    /// Path to a previously built .meshscale/output directory.
    pub output: PathBuf,
}

pub fn upload(_args: UploadArgs) -> Result<()> {
    bail!("phase E (R2 upload) is intentionally not implemented yet")
}
