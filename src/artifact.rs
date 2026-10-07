use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};
use tracing::{info, warn};
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

    let adapter_metadata = next_dir.join("meshscale-adapter.json");
    if metadata.version == 2 && !adapter_metadata.is_file() {
        bail!(
            "MeshScale Next.js adapter did not produce deployment metadata: {}",
            adapter_metadata.display()
        );
    }

    let next_package_json = project_dir
        .join("node_modules")
        .join("next")
        .join("package.json");
    if !next_package_json.is_file() {
        bail!(
            "installed Next.js package is missing: {}",
            next_package_json.display()
        );
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
        fs::remove_dir_all(&output_dir).with_context(|| {
            format!("failed to remove previous output {}", output_dir.display())
        })?;
    }

    let runtime_dir = output_dir.join("runtime");
    let static_dir = output_dir.join("static");
    fs::create_dir_all(&runtime_dir)
        .with_context(|| format!("failed to create {}", runtime_dir.display()))?;
    fs::create_dir_all(&static_dir)
        .with_context(|| format!("failed to create {}", static_dir.display()))?;

    let mut all_traced_files = collect_next_trace_files(&next_dir, &project_root)?;
    if all_traced_files.contains_key("function-entry.cjs") {
        bail!("project trace conflicts with the generated function-entry.cjs entrypoint");
    }
    collect_entrypoint_trace(project_dir, &project_root, &mut all_traced_files)?;
    if metadata.version == 2 {
        collect_adapter_assets(
            project_dir,
            &project_root,
            &adapter_metadata,
            &mut all_traced_files,
        )?;
    }
    if metadata.version == 2 {
        ensure_runtime_package_manifest(&project_root, &mut all_traced_files, "next")?;
        ensure_runtime_package_manifest(&project_root, &mut all_traced_files, "@next/routing")?;
    }
    materialize_package_dependencies(&project_root, &mut all_traced_files)?;

    info!(files = all_traced_files.len(), "collected runtime trace");

    copy_runtime_files(&runtime_dir, &all_traced_files)?;
    copy_required_runtime_files(project_dir, &runtime_dir)?;
    if adapter_metadata.is_file() {
        let destination = runtime_dir.join(".next").join("meshscale-adapter.json");
        fs::create_dir_all(
            destination
                .parent()
                .context("adapter metadata has no parent")?,
        )?;
        fs::copy(&adapter_metadata, &destination)
            .with_context(|| format!("failed to copy {}", adapter_metadata.display()))?;
    }
    copy_public_assets(project_dir, &static_dir)?;
    copy_next_static(project_dir, &static_dir)?;
    fs::write(
        runtime_dir.join("function-entry.cjs"),
        include_str!("function_entry.cjs"),
    )
    .context("failed to write MeshScale function entrypoint")?;

    let manifest = json!({
        "version": metadata.version,
        "framework": metadata.framework.as_str(),
        "runtime": {
            "type": "node",
            "command": "node",
            "entrypoint": "runtime/function-entry.cjs",
            "working_directory": "runtime",
            "args": []
        },
        "static": {
            "directory": "static"
        },
        "deployment": {
            "id": &metadata.build_id,
            "repository": &metadata.repository,
            "commit": &metadata.commit,
            "branch": &metadata.branch,
            "org_id": &metadata.org_id,
            "project_id": &metadata.project_id
        }
    });
    let mut manifest: crate::manifest::Manifest = serde_json::from_value(manifest)?;
    if metadata.version == 2 {
        crate::static_output::generate(project_dir, &output_dir, &mut manifest)?;
    }

    fs::write(
        output_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).context("failed to serialize manifest")?,
    )
    .context("failed to write manifest.json")?;

    validate_output(&output_dir)?;

    Ok(output_dir)
}

fn collect_next_trace_files(
    next_dir: &Path,
    project_root: &Path,
) -> Result<BTreeMap<String, PathBuf>> {
    let mut files = BTreeMap::new();

    for entry in WalkDir::new(next_dir).follow_links(false).into_iter() {
        let entry = entry.context("failed to walk Next.js trace directory")?;
        let path = entry.path();
        if path
            .strip_prefix(next_dir)
            .ok()
            .and_then(|relative| relative.components().next())
            .is_some_and(|component| component.as_os_str() == "standalone")
        {
            continue;
        }
        if !entry.file_type().is_file() || path.extension().and_then(|v| v.to_str()) != Some("json")
        {
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
            let (logical_path, resolved) = resolve_trace_file(project_root, trace_root, relative)
                .with_context(|| {
                format!(
                    "failed to resolve traced file {} from {}",
                    relative,
                    path.display()
                )
            })?;

            collect_trace_entry(&mut files, project_root, &logical_path, &resolved)?;
        }
    }

    Ok(files)
}

fn collect_entrypoint_trace(
    project_dir: &Path,
    project_root: &Path,
    files: &mut BTreeMap<String, PathBuf>,
) -> Result<()> {
    let mut entrypoint = tempfile::Builder::new()
        .prefix(".meshscale-function-entrypoint-")
        .suffix(".cjs")
        .tempfile_in(project_dir)
        .context("failed to stage production entrypoint for tracing")?;
    entrypoint.write_all(include_bytes!("function_entry.cjs"))?;
    entrypoint.flush()?;
    let entrypoint_source = entrypoint.path().canonicalize()?;
    let script = r#"
const { nodeFileTrace } = require('next/dist/compiled/@vercel/nft');
nodeFileTrace([process.argv[1]], {
  base: process.cwd(),
  processCwd: process.cwd(),
}).then(({ fileList, warnings }) => {
  console.log(JSON.stringify({ files: [...fileList], warnings: [...warnings].map(String) }));
}).catch((error) => { console.error(error); process.exit(1); });
"#;
    let mut command = Command::new("node");
    crate::upload::remove_credentials(&mut command);
    let output = command
        .args(["-e", script])
        .arg(
            entrypoint
                .path()
                .file_name()
                .context("staged entrypoint has no filename")?,
        )
        .current_dir(project_dir)
        .env("NODE_ENV", "production")
        .output()
        .context("failed to trace production entrypoint with Next.js bundled NFT")?;
    if !output.status.success() {
        bail!(
            "production entrypoint tracing failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[derive(serde::Deserialize)]
    struct Trace {
        files: Vec<String>,
        warnings: Vec<String>,
    }
    let trace: Trace = serde_json::from_slice(&output.stdout)
        .context("invalid production entrypoint NFT trace")?;
    for warning in trace.warnings {
        warn!(warning, "production entrypoint NFT warning");
    }
    for relative in trace.files {
        let (logical, source) = resolve_trace_file(project_root, project_dir, &relative)?;
        if source != entrypoint_source {
            collect_trace_entry(files, project_root, &logical, &source)?;
        }
    }
    entrypoint
        .close()
        .context("failed to remove staged production entrypoint")?;
    Ok(())
}

fn collect_trace_entry(
    files: &mut BTreeMap<String, PathBuf>,
    project_root: &Path,
    logical_path: &str,
    source: &Path,
) -> Result<()> {
    if let Some(existing) = files.get(logical_path) {
        if existing == source {
            return Ok(());
        }
        bail!(
            "Next.js NFT trace maps {} to multiple files: {} and {}",
            logical_path,
            existing.display(),
            source.display()
        );
    }
    // NFT records package directory links as well as individual files.
    for entry in WalkDir::new(source).follow_links(true) {
        let entry = entry
            .with_context(|| format!("failed to walk traced runtime entry {}", source.display()))?;
        let resolved = entry.path().canonicalize().with_context(|| {
            format!(
                "failed to resolve traced runtime entry {}",
                entry.path().display()
            )
        })?;
        if !resolved.starts_with(project_root) {
            bail!(
                "NFT trace points outside the build project root: {}",
                resolved.display()
            );
        }

        let relative = entry
            .path()
            .strip_prefix(source)
            .context("failed to calculate traced runtime path")?;
        let logical_path = if relative.as_os_str().is_empty() {
            logical_path.to_owned()
        } else {
            Path::new(logical_path)
                .join(relative)
                .to_string_lossy()
                .replace('\\', "/")
        };

        if let Some(existing) = files.insert(logical_path.clone(), resolved.clone())
            && existing != resolved
        {
            bail!(
                "Next.js NFT trace maps {} to multiple files: {} and {}",
                logical_path,
                existing.display(),
                resolved.display()
            );
        }
    }

    Ok(())
}

fn collect_adapter_assets(
    project_dir: &Path,
    project_root: &Path,
    metadata_path: &Path,
    files: &mut BTreeMap<String, PathBuf>,
) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Adapter {
        outputs: serde_json::Value,
    }

    let metadata: Adapter = serde_json::from_slice(&fs::read(metadata_path)?)
        .context("invalid MeshScale adapter metadata")?;
    let groups = ["pages", "pagesApi", "appPages", "appRoutes"];
    for group in groups {
        let Some(outputs) = metadata
            .outputs
            .get(group)
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for output in outputs {
            let Some(assets) = output.get("assets").and_then(serde_json::Value::as_object) else {
                continue;
            };
            for (logical, relative) in assets {
                let relative = relative
                    .as_str()
                    .context("adapter asset path is not a string")?;
                let source = project_dir
                    .join(relative)
                    .canonicalize()
                    .with_context(|| format!("adapter asset does not exist: {relative}"))?;
                let logical = Path::new(logical);
                ensure!(
                    logical.is_relative()
                        && logical.components().all(|component| {
                            matches!(component, std::path::Component::Normal(_))
                        }),
                    "invalid adapter asset path: {}",
                    logical.display()
                );
                let logical = logical.to_string_lossy().replace('\\', "/");
                collect_trace_entry(files, project_root, &logical, &source)?;
            }
        }
    }
    Ok(())
}

fn ensure_runtime_package_manifest(
    project_root: &Path,
    files: &mut BTreeMap<String, PathBuf>,
    package_name: &str,
) -> Result<()> {
    let source = project_root
        .join("node_modules")
        .join(package_name)
        .join("package.json");
    let source = source
        .canonicalize()
        .with_context(|| format!("required runtime package is missing: {package_name}"))?;
    let logical = Path::new("node_modules")
        .join(package_name)
        .join("package.json");
    let logical = logical.to_string_lossy().replace('\\', "/");
    collect_trace_entry(files, project_root, &logical, &source)
}

fn materialize_package_dependencies(
    project_root: &Path,
    files: &mut BTreeMap<String, PathBuf>,
) -> Result<()> {
    let traced = files.clone();
    for (logical, source) in &traced {
        let (logical, source) = if Path::new(logical)
            .file_name()
            .is_some_and(|name| name == "package.json")
        {
            (
                Path::new(logical)
                    .parent()
                    .context("package manifest has no parent")?
                    .to_string_lossy()
                    .replace('\\', "/"),
                source
                    .parent()
                    .context("resolved package manifest has no parent")?,
            )
        } else {
            (logical.clone(), source.as_path())
        };
        if source.is_dir()
            && source.join("package.json").is_file()
            && fs::read_link(project_root.join(&logical)).is_ok()
        {
            materialize_package_dependency_tree(
                project_root,
                &traced,
                files,
                &logical,
                source,
                &mut Vec::new(),
            )?;
        }
    }
    Ok(())
}

fn materialize_package_dependency_tree(
    project_root: &Path,
    traced: &BTreeMap<String, PathBuf>,
    files: &mut BTreeMap<String, PathBuf>,
    logical: &str,
    source: &Path,
    ancestors: &mut Vec<PathBuf>,
) -> Result<()> {
    // Dereferencing a pnpm package changes Node's module search ancestry. Recreate
    // its traced dependency context, not a global flattening that mixes versions.
    if ancestors.iter().any(|ancestor| ancestor == source) {
        return Ok(());
    }
    ancestors.push(source.to_path_buf());
    let package: serde_json::Value =
        serde_json::from_slice(&fs::read(source.join("package.json"))?)
            .with_context(|| format!("invalid package manifest in {}", source.display()))?;
    let mut names = std::collections::BTreeSet::new();
    for field in ["dependencies", "optionalDependencies", "peerDependencies"] {
        if let Some(dependencies) = package.get(field).and_then(serde_json::Value::as_object) {
            names.extend(dependencies.keys());
        }
    }
    for name in names {
        let components = name.split('/').collect::<Vec<_>>();
        if !((components.len() == 1 && !name.starts_with('@'))
            || (components.len() == 2 && name.starts_with('@')))
            || components.iter().any(|part| {
                part.is_empty() || *part == "." || *part == ".." || part.contains(['\\', ':', '\0'])
            })
        {
            bail!("invalid dependency name {name} in {}", source.display());
        }
        let mut dependency = None;
        for ancestor in source.ancestors() {
            let candidate = ancestor.join("node_modules").join(name);
            match candidate.canonicalize() {
                Ok(path) => {
                    if !path.starts_with(project_root) {
                        bail!(
                            "package dependency points outside the build project root: {}",
                            path.display()
                        );
                    }
                    dependency = Some(path);
                    break;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to resolve dependency {}", candidate.display())
                    });
                }
            }
            if ancestor == project_root {
                break;
            }
        }
        let Some(dependency) = dependency else {
            continue;
        };
        if ancestors.contains(&dependency) {
            continue;
        }
        let entries = traced
            .values()
            .filter_map(|path| {
                path.strip_prefix(&dependency)
                    .ok()
                    .map(|relative| (relative.to_path_buf(), path.clone()))
            })
            .collect::<BTreeMap<_, _>>();
        if entries.is_empty() {
            continue;
        }
        let alias = Path::new(logical)
            .join("node_modules")
            .join(name)
            .to_string_lossy()
            .replace('\\', "/");
        for (relative, path) in entries {
            let destination = if relative.as_os_str().is_empty() {
                alias.clone()
            } else {
                Path::new(&alias)
                    .join(relative)
                    .to_string_lossy()
                    .replace('\\', "/")
            };
            if let Some(existing) = files.insert(destination.clone(), path.clone())
                && existing != path
            {
                bail!("dependency context maps {destination} to multiple files");
            }
        }
        if dependency.join("package.json").is_file() {
            materialize_package_dependency_tree(
                project_root,
                traced,
                files,
                &alias,
                &dependency,
                ancestors,
            )?;
        }
    }
    ancestors.pop();
    Ok(())
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

fn copy_runtime_files(runtime_dir: &Path, files: &BTreeMap<String, PathBuf>) -> Result<()> {
    for (relative, source) in files {
        let destination = runtime_dir.join(relative);
        let metadata = fs::metadata(source)
            .with_context(|| format!("failed to inspect traced file {}", source.display()))?;

        if metadata.is_dir() {
            fs::create_dir_all(&destination)
                .with_context(|| format!("failed to create {}", destination.display()))?;
            continue;
        }

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

fn copy_required_runtime_files(project_dir: &Path, runtime_dir: &Path) -> Result<()> {
    let source = project_dir.join("package.json");
    fs::copy(&source, runtime_dir.join("package.json"))
        .with_context(|| format!("failed to copy {}", source.display()))?;
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
        for entry in WalkDir::new(&next_static).follow_links(false) {
            let entry = entry.context("failed to inspect Next.js static assets")?;
            let destination = static_dir
                .join("_next")
                .join("static")
                .join(entry.path().strip_prefix(&next_static)?);
            if entry.file_type().is_file() && destination.exists() {
                bail!(
                    "public assets conflict with Next.js static asset {}",
                    destination.display()
                );
            }
        }
        copy_directory_contents(&next_static, &static_dir.join("_next").join("static"))?;
    }
    Ok(())
}

fn copy_directory_contents(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;

    for entry in WalkDir::new(source).follow_links(false).into_iter() {
        let entry = entry.context("failed to walk static asset directory")?;
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
        bail!(
            "output contains a symbolic link or Windows junction: {}",
            source_path.display()
        );
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

pub(crate) fn validate_output(output_dir: &Path) -> Result<()> {
    validate_output_mode(output_dir, false)
}

pub(crate) fn validate_server_output(output_dir: &Path) -> Result<()> {
    validate_output_mode(output_dir, true)
}

fn validate_output_mode(output_dir: &Path, server_only: bool) -> Result<()> {
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
    if !server_only && !output_dir.join("static").is_dir() {
        bail!("output is missing the static directory");
    }

    for entry in WalkDir::new(output_dir).follow_links(false).into_iter() {
        let entry = entry.context("failed to walk output directory")?;
        let metadata = fs::symlink_metadata(entry.path()).with_context(|| {
            format!("failed to inspect output entry {}", entry.path().display())
        })?;
        if metadata.file_type().is_symlink() || fs::read_link(entry.path()).is_ok() {
            bail!(
                "output contains a symbolic link or Windows junction and is not relocatable: {}",
                entry.path().display()
            );
        }
    }

    if server_only {
        crate::manifest::load_server(output_dir)?;
    } else {
        crate::manifest::load(output_dir)?;
    }
    info!("validated .meshscale/output");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Framework, PackageManager};
    use tempfile::TempDir;

    fn write_file(path: &Path, content: &str) -> Result<()> {
        fs::create_dir_all(path.parent().context("test file has no parent")?)?;
        fs::write(path, content)?;
        Ok(())
    }

    #[cfg(windows)]
    fn link_directory(target: &Path, link: &Path) -> Result<()> {
        let status =
            std::process::Command::new(std::env::var_os("COMSPEC").context("missing COMSPEC")?)
                .args(["/c", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()?;
        anyhow::ensure!(
            status.status.success(),
            "failed to create test junction: {} {}",
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        );
        Ok(())
    }

    #[cfg(unix)]
    fn link_directory(target: &Path, link: &Path) -> Result<()> {
        std::os::unix::fs::symlink(target, link)?;
        Ok(())
    }

    fn write_trace(project: &Path, files: &[&str]) -> Result<()> {
        write_file(
            &project.join(".next").join("next-server.js.nft.json"),
            &json!({ "version": 1, "files": files }).to_string(),
        )
    }

    #[test]
    fn materializes_pnpm_package_directory_links_in_complete_output() -> Result<()> {
        let workspace = TempDir::new()?;
        let project = workspace.path().join("repo");
        let modules = project.join("node_modules");
        let next = modules
            .join(".pnpm")
            .join("next@16.3.4")
            .join("node_modules")
            .join("next");
        let env = modules
            .join(".pnpm")
            .join("@next+env@16.3.4")
            .join("node_modules")
            .join("@next")
            .join("env");
        write_file(&next.join("package.json"), r#"{"name":"next"}"#)?;
        write_file(&next.join("dist").join("bin").join("next"), "next cli")?;
        write_file(
            &next
                .join("dist")
                .join("compiled")
                .join("@vercel")
                .join("nft")
                .join("index.js"),
            "exports.nodeFileTrace = async ([file]) => ({fileList: new Set([file, 'node_modules/next/package.json']), warnings: new Set()});",
        )?;
        write_file(&env.join("package.json"), r#"{"name":"@next/env"}"#)?;
        write_file(&env.join("dist").join("index.js"), "env module")?;
        fs::create_dir_all(env.join("empty"))?;
        fs::create_dir_all(modules.join("@next"))?;
        link_directory(&next, &modules.join("next"))?;
        link_directory(&env, &modules.join("@next").join("env"))?;
        write_file(&project.join("package.json"), "{}")?;
        write_file(
            &project.join(".next").join("required-server-files.json"),
            r#"{"config":{}}"#,
        )?;
        write_file(
            &project.join(".next").join("static").join("app.js"),
            "static",
        )?;
        write_file(&project.join("public").join("asset.txt"), "public")?;
        write_trace(
            &project,
            &[
                "../node_modules/next",
                "../node_modules/@next/env",
                "../node_modules/@next/env/dist/index.js",
                "../node_modules/.pnpm/@next+env@16.3.4/node_modules/@next/env",
            ],
        )?;
        write_file(
            &project
                .join(".next")
                .join("standalone")
                .join("ignored.nft.json"),
            r#"{"files":["missing-file"]}"#,
        )?;

        let metadata = BuildMetadata {
            version: 1,
            framework: Framework::NextJs,
            package_manager: PackageManager::Pnpm,
            build_id: "test".to_owned(),
            repository: "test/repo".to_owned(),
            commit: "test".to_owned(),
            branch: "main".to_owned(),
            org_id: None,
            project_id: None,
        };
        let output = create_output(&project, &metadata, &workspace.path().join("output"))?;
        let runtime = output.join("runtime");
        assert_eq!(
            fs::read_to_string(
                runtime
                    .join("node_modules")
                    .join("@next")
                    .join("env")
                    .join("dist")
                    .join("index.js")
            )?,
            "env module"
        );
        assert!(
            runtime
                .join("node_modules")
                .join("@next")
                .join("env")
                .join("empty")
                .is_dir()
        );
        assert!(
            runtime
                .join("node_modules")
                .join("next")
                .join("dist")
                .join("bin")
                .join("next")
                .is_file()
        );
        assert!(!runtime.join(".next").join("standalone").exists());
        assert_eq!(
            fs::read_to_string(output.join("static").join("asset.txt"))?,
            "public"
        );
        assert_eq!(
            fs::read_to_string(
                output
                    .join("static")
                    .join("_next")
                    .join("static")
                    .join("app.js")
            )?,
            "static"
        );

        fs::remove_dir_all(&project)?;
        validate_output(&output)?;
        assert_eq!(
            fs::read_to_string(
                runtime
                    .join("node_modules")
                    .join(".pnpm")
                    .join("@next+env@16.3.4")
                    .join("node_modules")
                    .join("@next")
                    .join("env")
                    .join("dist")
                    .join("index.js")
            )?,
            "env module"
        );
        Ok(())
    }

    #[test]
    fn copies_regular_traced_files_without_changing_logical_paths() -> Result<()> {
        let workspace = TempDir::new()?;
        write_file(&workspace.path().join("module.js"), "module")?;
        write_trace(workspace.path(), &["../module.js"])?;
        let root = workspace.path().canonicalize()?;
        let files = collect_next_trace_files(&root.join(".next"), &root)?;
        assert_eq!(files.len(), 1);
        assert_eq!(files["module.js"], root.join("module.js"));
        let output = workspace.path().join("runtime");
        copy_runtime_files(&output, &files)?;
        assert_eq!(fs::read_to_string(output.join("module.js"))?, "module");
        Ok(())
    }

    #[test]
    fn materializes_nested_directory_links() -> Result<()> {
        let workspace = TempDir::new()?;
        let package = workspace.path().join("package");
        let dependency = workspace.path().join("dependency");
        fs::create_dir_all(&package)?;
        write_file(&dependency.join("index.js"), "dependency")?;
        link_directory(&dependency, &package.join("linked"))?;
        write_trace(workspace.path(), &["../package"])?;
        let root = workspace.path().canonicalize()?;
        let files = collect_next_trace_files(&root.join(".next"), &root)?;
        let output = workspace.path().join("runtime");
        copy_runtime_files(&output, &files)?;
        assert_eq!(
            fs::read_to_string(output.join("package").join("linked").join("index.js"))?,
            "dependency"
        );
        assert!(fs::read_link(output.join("package").join("linked")).is_err());
        Ok(())
    }

    #[test]
    fn rejects_directory_links_outside_project() -> Result<()> {
        let workspace = TempDir::new()?;
        let outside = TempDir::new()?;
        link_directory(outside.path(), &workspace.path().join("outside"))?;
        write_trace(workspace.path(), &["../outside"])?;
        let root = workspace.path().canonicalize()?;
        let error = collect_next_trace_files(&root.join(".next"), &root).unwrap_err();
        assert!(format!("{error:#}").contains("outside the build project root"));
        Ok(())
    }

    #[test]
    fn rejects_nested_directory_links_outside_project() -> Result<()> {
        let workspace = TempDir::new()?;
        let outside = TempDir::new()?;
        let package = workspace.path().join("package");
        fs::create_dir_all(&package)?;
        link_directory(outside.path(), &package.join("outside"))?;
        write_trace(workspace.path(), &["../package"])?;
        let root = workspace.path().canonicalize()?;
        let error = collect_next_trace_files(&root.join(".next"), &root).unwrap_err();
        assert!(format!("{error:#}").contains("outside the build project root"));
        Ok(())
    }

    #[test]
    fn rejects_cyclic_directory_links() -> Result<()> {
        let workspace = TempDir::new()?;
        let package = workspace.path().join("package");
        fs::create_dir_all(&package)?;
        link_directory(&package, &package.join("cycle"))?;
        write_trace(workspace.path(), &["../package"])?;
        let root = workspace.path().canonicalize()?;
        let error = collect_next_trace_files(&root.join(".next"), &root).unwrap_err();
        assert!(format!("{error:#}").contains("loop"));
        Ok(())
    }

    #[test]
    fn rejects_missing_traced_files_and_parent_escapes() -> Result<()> {
        let workspace = TempDir::new()?;
        write_trace(workspace.path(), &["../missing"])?;
        let root = workspace.path().canonicalize()?;
        let error = collect_next_trace_files(&root.join(".next"), &root).unwrap_err();
        assert!(format!("{error:#}").contains("failed to canonicalize traced file"));
        write_trace(workspace.path(), &["../../outside"])?;
        let error = collect_next_trace_files(&root.join(".next"), &root).unwrap_err();
        assert!(format!("{error:#}").contains("escapes the build project root"));
        Ok(())
    }

    #[test]
    fn preserves_pnpm_transitive_dependency_context_without_hoisting_versions() -> Result<()> {
        let workspace = TempDir::new()?;
        let project = workspace.path().join("repo");
        let modules = project.join("node_modules");
        let store = modules.join(".pnpm");
        let next = store.join("next@1").join("node_modules").join("next");
        let helpers = store
            .join("helpers@1")
            .join("node_modules")
            .join("@swc")
            .join("helpers");
        let tslib = store.join("tslib@1").join("node_modules").join("tslib");
        let other = store.join("tslib@2").join("node_modules").join("tslib");
        write_file(
            &next.join("package.json"),
            r#"{"dependencies":{"@swc/helpers":"1"}}"#,
        )?;
        write_file(
            &next.join("index.js"),
            "module.exports = require('@swc/helpers/_/_interop_require_default');",
        )?;
        write_file(
            &helpers.join("package.json"),
            r#"{"dependencies":{"tslib":"1"}}"#,
        )?;
        write_file(
            &helpers
                .join("_")
                .join("_interop_require_default")
                .join("index.js"),
            "module.exports = require('tslib');",
        )?;
        write_file(&tslib.join("package.json"), r#"{"main":"index.js"}"#)?;
        write_file(
            &tslib.join("index.js"),
            "module.exports = 'correct version';",
        )?;
        write_file(&other.join("package.json"), r#"{"main":"index.js"}"#)?;
        write_file(&other.join("index.js"), "module.exports = 'wrong version';")?;
        fs::create_dir_all(next.parent().unwrap().join("@swc"))?;
        link_directory(
            &helpers,
            &next.parent().unwrap().join("@swc").join("helpers"),
        )?;
        link_directory(
            &tslib,
            &helpers.parent().unwrap().parent().unwrap().join("tslib"),
        )?;
        link_directory(&next, &modules.join("next"))?;
        link_directory(&other, &modules.join("tslib"))?;
        write_trace(
            &project,
            &[
                "../node_modules/next/package.json",
                "../node_modules/next/index.js",
                "../node_modules/.pnpm/helpers@1/node_modules/@swc/helpers",
                "../node_modules/.pnpm/tslib@1/node_modules/tslib",
                "../node_modules/tslib",
            ],
        )?;
        let root = project.canonicalize()?;
        let mut files = collect_next_trace_files(&root.join(".next"), &root)?;
        materialize_package_dependencies(&root, &mut files)?;
        let output = workspace.path().join("runtime");
        copy_runtime_files(&output, &files)?;
        fs::remove_dir_all(&project)?;
        let output = Command::new("node").args([
            "-e", "if (require('./node_modules/next') !== 'correct version') process.exit(1); if (require('./node_modules/tslib') !== 'wrong version') process.exit(2);",
        ]).current_dir(&output).output()?;
        anyhow::ensure!(
            output.status.success(),
            "relocated pnpm dependency resolution failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
}
