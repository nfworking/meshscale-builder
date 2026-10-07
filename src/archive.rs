use crate::{artifact, manifest, static_output};
use anyhow::{Context, Result, ensure};
use clap::Args;
use serde::Serialize;
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};
use walkdir::WalkDir;
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

#[derive(Args, Debug)]
pub struct PackageArgs {
    pub output: PathBuf,
    /// Assert/bind the project ID used in the archive name.
    #[arg(long)]
    pub project_id: String,
}

#[derive(Debug, Serialize)]
pub struct ArchiveResult {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

pub fn package(args: PackageArgs) -> Result<()> {
    artifact::validate_output(&args.output)?;
    let mut metadata = manifest::load(&args.output)?;
    metadata.validate_v2()?;
    crate::upload::validate_id("project-id", &args.project_id)?;
    ensure!(
        metadata
            .deployment
            .project_id
            .as_ref()
            .is_none_or(|project| project == &args.project_id),
        "project ID mismatch"
    );
    metadata.deployment.project_id = Some(args.project_id);
    metadata.server = Some(manifest::Server {
        target_id: metadata.target_id(),
    });
    metadata
        .archive
        .as_mut()
        .context("missing archive")?
        .filename = metadata.archive_name()?;
    metadata.validate_v2()?;
    static_output::verify_inventory(&args.output, &metadata)?;
    let staging = tempfile::TempDir::new()?;
    snapshot(&args.output, staging.path())?;
    manifest::write(staging.path(), &metadata)?;
    static_output::verify_inventory(staging.path(), &metadata)?;
    let destination = destination(&args.output, &metadata)?;
    let result = create(staging.path(), &destination)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({"status":"success", "archive":result}))?
    );
    Ok(())
}

pub fn destination(output: &Path, manifest: &manifest::Manifest) -> Result<PathBuf> {
    let output = output.canonicalize()?;
    let name = manifest
        .archive_name()?
        .context("project ID required for server packaging")?;
    let path = output.parent().context("output has no parent")?.join(name);
    ensure!(
        !path.exists(),
        "server archive already exists: {}; use a different output parent directory",
        path.display()
    );
    Ok(path)
}

pub fn snapshot(source: &Path, destination: &Path) -> Result<()> {
    let root = source.canonicalize()?;
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let name = relative
            .to_str()
            .context("artifact path must be UTF-8")?
            .replace('\\', "/");
        manifest::validate_relative(&name)?;
        ensure!(
            matches!(
                name.split('/').next(),
                Some("runtime" | "static" | "manifest.json")
            ),
            "unexpected artifact entry {name}"
        );
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink() && fs::read_link(entry.path()).is_err(),
            "artifact contains a link: {name}"
        );
        ensure!(
            entry.path().canonicalize()?.starts_with(&root),
            "artifact escapes output"
        );
        if metadata.is_dir() {
            fs::create_dir_all(destination.join(relative))?;
        } else {
            ensure!(metadata.is_file(), "unsupported artifact entry {name}");
            fs::create_dir_all(
                destination
                    .join(relative)
                    .parent()
                    .context("snapshot path has no parent")?,
            )?;
            fs::copy(entry.path(), destination.join(relative))?;
            fs::set_permissions(destination.join(relative), metadata.permissions())?;
        }
    }
    Ok(())
}

pub fn create(source: &Path, destination: &Path) -> Result<ArchiveResult> {
    artifact::validate_server_output(source)?;
    let manifest = manifest::load_server(source)?;
    manifest.validate_v2()?;
    ensure!(
        !destination.exists(),
        "server archive already exists: {}",
        destination.display()
    );
    let parent = destination.parent().context("archive has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut writer = ZipWriter::new(temporary.as_file_mut());
        let mut entries = WalkDir::new(source)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.path() != source.join("static"))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.path().to_path_buf());
        for entry in entries {
            let relative = entry.path().strip_prefix(source)?;
            if relative.as_os_str().is_empty() {
                continue;
            }
            let name = relative
                .to_str()
                .context("archive path must be UTF-8")?
                .replace('\\', "/");
            manifest::validate_relative(&name)?;
            let metadata = fs::symlink_metadata(entry.path())?;
            ensure!(
                !metadata.file_type().is_symlink() && fs::read_link(entry.path()).is_err(),
                "archive contains a link"
            );
            let mode = permissions(&metadata);
            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .last_modified_time(zip::DateTime::default())
                .unix_permissions(mode)
                .large_file(metadata.len() >= u32::MAX as u64);
            if metadata.is_dir() {
                writer.add_directory(format!("{name}/"), options)?;
            } else {
                ensure!(metadata.is_file(), "unsupported archive entry");
                writer.start_file(name, options)?;
                io::copy(&mut fs::File::open(entry.path())?, &mut writer)?;
            }
        }
        writer.finish()?;
    }
    temporary.as_file().sync_all()?;
    // Reopen through the reader to verify names and every compressed entry's CRC.
    {
        let mut archive = ZipArchive::new(fs::File::open(temporary.path())?)?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            ensure!(
                entry.enclosed_name().is_some() && !entry.name().starts_with("static/"),
                "invalid server archive entry"
            );
            let mut buffer = [0; 64 * 1024];
            while entry.read(&mut buffer)? != 0 {}
        }
    }
    let (bytes, sha256) = static_output::digest_file(temporary.path())?;
    temporary
        .persist_noclobber(destination)
        .map_err(|error| error.error)
        .context("failed to atomically publish server ZIP")?;
    Ok(ArchiveResult {
        path: destination.to_path_buf(),
        bytes,
        sha256,
    })
}

#[cfg(unix)]
fn permissions(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn permissions(metadata: &fs::Metadata) -> u32 {
    if metadata.is_dir() { 0o755 } else { 0o644 }
}
