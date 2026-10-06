use anyhow::{bail, Context, Result};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tracing::info;
use walkdir::WalkDir;

use crate::BuildMetadata;

const TRACE_HELPER: &str = "scripts/trace-runtime.cjs";

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

    let traced_files = run_runtime_trace(&project_root, &next_package_json)?;
    let next_trace_files = collect_next_trace_files(&next_dir, &project_root)?;
    let mut all_traced_files = BTreeSet::new();
    all_traced_files.extend(traced_files);
    all_traced_files.extend(next_trace_files);

    info!(files = all_traced_files.len(), "collected runtime trace");

    copy_runtime_files(&project_root, &runtime_dir, &all_traced_files)?;
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
            "args": ["start"]
        },
        "static": {
            "directory": "static"
        },
        "deployment": {
            "id": metadata.build_id,
            "repository": metadata.repository,
            "commit": metadata.commit,
            "branch": metadata.branch
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

fn run_runtime_trace(project_root: &Path, next_package_json: &Path) -> Result<BTreeSet<String>> {
    let helper = locate_trace_helper()?;

    let output = Command::new(node_executable())
        .arg(&helper)
        .arg(project_root)
        .arg(next_package_json)
        .current_dir(project_root)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("failed to start Node NFT helper {}", helper.display()))?;

    if !output.status.success() {
        bail!(
            "Node NFT helper failed with exit status {}",
            output
                .status
                .code()
                .map_or_else(|| "terminated by signal".to_owned(), |code| code.to_string())
        );
    }

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("NFT helper returned invalid JSON")?;

    let files = value
        .get("files")
        .and_then(serde_json::Value::as_array)
        .context("NFT helper response is missing a files array")?;

    files
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("NFT helper returned a non-string file path")
        })
        .collect()
}

fn locate_trace_helper() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let helper = manifest_dir.join(TRACE_HELPER);
    if !helper.is_file() {
        bail!("NFT helper is missing from the builder repository: {}", helper.display());
    }
    Ok(helper)
}

fn node_executable() -> &'static str {
    if cfg!(windows) {
        "node.exe"
    } else {
        "node"
    }
}

fn collect_next_trace_files(next_dir: &Path, project_root: &Path) -> Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();

    for entry in WalkDir::new(next_dir)
        .follow_links(false)
        .into_iter()
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
            let resolved = trace_root.join(relative).canonicalize().with_context(|| {
                format!(
                    "failed to resolve traced file {} from {}",
                    relative,
                    path.display()
                )
            })?;

            files.insert(relative_project_path(project_root, &resolved)?);
        }
    }

    Ok(files)
}

fn relative_project_path(project_root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(project_root).with_context(|| {
        format!(
            "NFT trace points outside the build project root: {}",
            path.display()
        )
    })?;

    if relative.components().any(|component| {
        matches!(component, std::path::Component::ParentDir)
    }) {
        bail!("NFT trace produced a parent-directory path: {}", path.display());
    }

    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn copy_runtime_files(project_root: &Path, runtime_dir: &Path, files: &BTreeSet<String>) -> Result<()> {
    for relative in files {
        let source = project_root.join(relative);
        let destination = runtime_dir.join(relative);

        let metadata = fs::symlink_metadata(&source)
            .with_context(|| format!("failed to inspect traced file {}", source.display()))?;

        if metadata.file_type().is_symlink() || fs::read_link(&source).is_ok() {
            bail!(
                "NFT trace contains a symbolic link or Windows junction; relocatable output requires a real file: {}",
                source.display()
            );
        }

        if metadata.is_dir() {
            continue;
        }
        if !metadata.is_file() {
            bail!("NFT trace contains unsupported filesystem entry: {}", source.display());
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        fs::copy(&source, &destination).with_context(|| {
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
    copy_tree_excluding_cache(&source, &destination)
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

fn copy_tree_excluding_cache(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;

    let walker = WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.path().file_name().and_then(|name| name.to_str()) != Some("cache")
        });

    for entry in walker.filter_map(std::result::Result::ok) {
        copy_entry(source, destination, entry.path())?;
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
