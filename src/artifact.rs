use anyhow::{bail, Context, Result};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use tracing::info;
use walkdir::WalkDir;

use crate::BuildMetadata;

pub fn create_output(
    project_dir: &Path,
    metadata: &BuildMetadata,
    output_dir: &Path,
) -> Result<PathBuf> {
    let next_dir = project_dir.join(".next");
    if !next_dir.is_dir() {
        bail!(
            "Next.js build completed but .next was not produced: {}",
            next_dir.display()
        );
    }

    let next_server_trace = next_dir.join("next-server.js.nft.json");
    if !next_server_trace.is_file() {
        bail!(
            "Next.js production server trace was not produced: {}",
            next_server_trace.display()
        );
    }

    let next_package_json = project_dir.join("node_modules").join("next").join("package.json");
    if !next_package_json.is_file() {
        bail!("installed Next.js package is missing: {}", next_package_json.display());
    }

    let project_root = project_dir
        .canonicalize()
        .context("failed to canonicalize build project root")?;

    let output_dir = if output_dir.is_absolute() {
        output_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to resolve builder working directory")?
            .join(output_dir)
    };

    if output_dir.exists() {
        fs::remove_dir_all(&output_dir)
            .with_context(|| format!("failed to remove previous output {}", output_dir.display()))?;
    }

    let runtime_dir = output_dir.join("runtime");
    let static_dir = output_dir.join("static");
    fs::create_dir_all(&runtime_dir)
        .with_context(|| format!("failed to create {}", runtime_dir.display()))?;
    fs::create_dir_all(&static_dir)
        .with_context(|| format!("failed to create {}", static_dir.display()))?;

    let all_traced_files = collect_next_trace_files(&next_dir, &project_root)?;

    info!(files = all_traced_files.len(), "collected runtime trace");

    copy_runtime_files(&runtime_dir, &all_traced_files)?;
    copy_next_runtime(project_dir, &runtime_dir)?;
    copy_required_runtime_files(project_dir, &runtime_dir)?;
    copy_public_assets(project_dir, &static_dir)?;
    copy_next_static(project_dir, &static_dir)?;

    let manifest = json!({
        "version": metadata.version,
        "framework": metadata.framework.as_str(),
        "runtime": {
            "type": "node",
            "command": "node",
            "entrypoint": "runtime/node_modules/next/dist/bin/next",
            "working_directory": "runtime",
            "args": ["start"]
        },
        "static": {
            "directory": "static"
        },
        "deployment": {
            "id": &metadata.build_id,
            "repository": &metadata.repository,
            "commit": &metadata.commit,
            "branch": &metadata.branch
        }
    });

    fs::write(
        output_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).context("failed to serialize manifest")?,
    )
    .context("failed to write manifest.json")?;

    validate_output(&output_dir)?;

    Ok(output_dir)
}

fn collect_next_trace_files(next_dir: &Path, project_root: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut files = BTreeMap::new();

    for entry in WalkDir::new(next_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_generated_standalone_tree(entry.path(), next_dir))
        .filter_map(std::result::Result::ok)
    {
        let path = entry.path();
        if !entry.file_type().is_file() || path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        if !path
            .file_name()
            .and_then(|v| v.to_str())
            .is_some_and(|name| name.ends_with(".nft.json"))
        {
            continue;
        }

        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read trace {}", path.display()))?;
        let trace: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("invalid NFT trace {}", path.display()))?;

        let trace_files = trace
            .get("files")
            .and_then(serde_json::Value::as_array)
            .with_context(|| format!("trace {} is missing files", path.display()))?;

        let trace_root = path
            .parent()
            .context("trace file has no parent directory")?;

        for trace_file in trace_files {
            let relative = trace_file
                .as_str()
                .context("Next.js NFT trace contains a non-string path")?;
            let (logical_path, resolved) =
                resolve_trace_file(project_root, trace_root, relative).with_context(|| {
                    format!(
                        "failed to resolve traced file {} from {}",
                        relative,
                        path.display()
                    )
                })?;

            if let Some(existing) = files.insert(logical_path.clone(), resolved.clone()) {
                if existing != resolved {
                    bail!(
                        "Next.js NFT trace maps {} to multiple files: {} and {}",
                        logical_path,
                        existing.display(),
                        resolved.display()
                    );
                }
            }
        }
    }

    Ok(files)
}

fn resolve_trace_file(
    project_root: &Path,
    trace_root: &Path,
    relative: &str,
) -> Result<(String, PathBuf)> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute() {
        bail!("NFT trace contains an absolute path: {}", relative);
    }

    let trace_root = trace_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize trace root {}", trace_root.display()))?;

    let trace_root_relative = trace_root.strip_prefix(project_root).with_context(|| {
        format!(
            "NFT trace root is outside the build project root: {}",
            trace_root.display()
        )
    })?;

    let mut logical_components = trace_root_relative.components().collect::<Vec<_>>();

    for component in relative_path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(value) => {
                logical_components.push(std::path::Component::Normal(value));
            }
            std::path::Component::ParentDir => {
                if logical_components.pop().is_none() {
                    bail!("NFT trace escapes the build project root: {}", relative);
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                bail!("NFT trace contains an invalid path: {}", relative);
            }
        }
    }

    let logical_path = logical_components
        .iter()
        .collect::<PathBuf>()
        .to_string_lossy()
        .replace('\\', "/");

    let resolved = trace_root.join(relative).canonicalize().with_context(|| {
        format!(
            "failed to canonicalize traced file {} from {}",
            relative,
            trace_root.display()
        )
    })?;

    if !resolved.starts_with(project_root) {
        bail!(
            "NFT trace points outside the build project root: {}",
            resolved.display()
        );
    }

    Ok((logical_path, resolved))
}

fn copy_runtime_files(
    runtime_dir: &Path,
    files: &BTreeMap<String, PathBuf>,
) -> Result<()> {
    for (relative, source) in files {
        let destination = runtime_dir.join(relative);
        let metadata = fs::metadata(source)
            .with_context(|| format!("failed to inspect traced file {}", source.display()))?;

        if !metadata.is_file() {
            bail!(
                "NFT trace contains unsupported filesystem entry: {}",
                source.display()
            );
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        fs::copy(source, &destination).with_context(|| {
            format!(
                "failed to copy traced runtime file {} to {}",
                source.display(),
                destination.display()
            )
        })?;
    }

    Ok(())
}

fn copy_next_runtime(project_dir: &Path, runtime_dir: &Path) -> Result<()> {
    let source = project_dir.join(".next");
    let destination = runtime_dir.join(".next");
    copy_tree_excluding_generated(&source, &destination)
}

fn is_generated_standalone_tree(path: &Path, next_dir: &Path) -> bool {
    path.strip_prefix(next_dir)
        .ok()
        .and_then(|relative| relative.components().next())
        .is_some_and(|component| component.as_os_str() == "standalone")
}

fn copy_tree_excluding_generated(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;

    let walker = WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.path().file_name().and_then(|name| name.to_str());
            name != Some("cache") && name != Some("standalone")
        });

    for entry in walker.filter_map(std::result::Result::ok) {
        copy_entry(source, destination, entry.path())?;
    }

    Ok(())
}

fn copy_required_runtime_files(project_dir: &Path, runtime_dir: &Path) -> Result<()> {
    for name in [
        "package.json",
        "next.config.js",
        "next.config.mjs",
        "next.config.cjs",
        "next.config.ts",
    ] {
        let source = project_dir.join(name);
        if source.is_file() {
            fs::copy(&source, runtime_dir.join(name))
                .with_context(|| format!("failed to copy {}", source.display()))?;
        }
    }

    Ok(())
}

fn copy_public_assets(project_dir: &Path, static_dir: &Path) -> Result<()> {
    let public = project_dir.join("public");
    if public.is_dir() {
        copy_directory_contents(&public, static_dir)?;
    }
    Ok(())
}

fn copy_next_static(project_dir: &Path, static_dir: &Path) -> Result<()> {
    let next_static = project_dir.join(".next").join("static");
    if next_static.is_dir() {
        copy_directory_contents(&next_static, &static_dir.join("_next").join("static"))?;
    }
    Ok(())
}

fn copy_directory_contents(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;

    for entry in WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        copy_entry(source, destination, entry.path())?;
    }

    Ok(())
}

fn copy_entry(source: &Path, destination: &Path, source_path: &Path) -> Result<()> {
    let relative = source_path
        .strip_prefix(source)
        .context("failed to calculate copied path")?;
    let destination_path = destination.join(relative);
    let metadata = fs::symlink_metadata(source_path)
        .with_context(|| format!("failed to inspect {}", source_path.display()))?;

    if metadata.file_type().is_symlink() || fs::read_link(source_path).is_ok() {
        bail!("output contains a symbolic link or Windows junction: {}", source_path.display());
    }

    if metadata.is_dir() {
        fs::create_dir_all(&destination_path)?;
    } else if metadata.is_file() {
        if let Some(parent) = destination_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source_path, &destination_path)?;
    }

    Ok(())
}

fn validate_output(output_dir: &Path) -> Result<()> {
    let manifest = output_dir.join("manifest.json");
    let runtime_next = output_dir
        .join("runtime")
        .join("node_modules")
        .join("next")
        .join("package.json");

    if !manifest.is_file() {
        bail!("output is missing manifest.json");
    }
    if !runtime_next.is_file() {
        bail!("output runtime is missing node_modules/next/package.json");
    }
    if !output_dir.join("runtime").join(".next").is_dir() {
        bail!("output runtime is missing the Next.js .next directory");
    }
    if !output_dir.join("static").is_dir() {
        bail!("output is missing the static directory");
    }

    for entry in WalkDir::new(output_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        let metadata = fs::symlink_metadata(entry.path())
            .with_context(|| format!("failed to inspect output entry {}", entry.path().display()))?;
        if metadata.file_type().is_symlink() || fs::read_link(entry.path()).is_ok() {
            bail!(
                "output contains a symbolic link or Windows junction and is not relocatable: {}",
                entry.path().display()
            );
        }
    }

    info!("validated .meshscale/output");
    Ok(())
}
