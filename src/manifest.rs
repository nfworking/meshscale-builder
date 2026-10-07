use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u8,
    pub framework: String,
    pub runtime: Runtime,
    pub r#static: Static,
    pub deployment: Deployment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<crate::routing::Routing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Server>,
    /// Ignored: manifests built before server ZIP packaging was removed still carry this field.
    #[serde(default, rename = "archive", skip_serializing)]
    _legacy_archive: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub r#type: String,
    pub command: String,
    pub entrypoint: PathBuf,
    pub working_directory: PathBuf,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Static {
    pub directory: PathBuf,
    #[serde(default)]
    pub objects: BTreeMap<String, StaticObject>,
    #[serde(default)]
    pub storage: Option<Storage>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub id: String,
    pub repository: String,
    pub commit: String,
    pub branch: String,
    pub org_id: Option<String>,
    pub project_id: Option<String>,
    #[serde(default)]
    pub next_build_id: Option<String>,
    #[serde(default)]
    pub next_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StaticObject {
    pub bytes: u64,
    pub sha256: String,
    pub content_type: String,
    pub cache_control: String,
    pub last_modified: String,
    pub class: String,
    pub status: u16,
    pub headers: BTreeMap<String, String>,
}

impl StaticObject {
    pub fn etag(&self) -> String {
        format!("\"{}\"", self.sha256)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub provider: String,
    pub bucket: String,
    pub prefix: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub target_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    pub node_version: String,
    pub node_abi: String,
}

pub fn load(output: &Path) -> Result<Manifest> {
    load_mode(output, false)
}

pub fn load_server(output: &Path) -> Result<Manifest> {
    load_mode(output, true)
}

fn load_mode(output: &Path, server_only: bool) -> Result<Manifest> {
    let path = output.join("manifest.json");
    let manifest: Manifest = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .context("invalid manifest.json")?;
    ensure!(
        matches!(manifest.version, 1 | 2),
        "unsupported manifest version: {}",
        manifest.version
    );
    ensure!(
        manifest.framework == "nextjs",
        "unsupported manifest framework: {}",
        manifest.framework
    );
    ensure!(
        manifest.runtime.r#type == "node" && manifest.runtime.command == "node",
        "manifest must use the Node runtime"
    );
    let legacy_cli = manifest.runtime.entrypoint
        == Path::new("runtime")
            .join("node_modules")
            .join("next")
            .join("dist")
            .join("bin")
            .join("next")
        && manifest.runtime.args == ["start"];
    let function_runtime = manifest.runtime.entrypoint
        == Path::new("runtime").join("function-entry.cjs")
        && manifest.runtime.args.is_empty();
    let production_server = manifest.runtime.entrypoint
        == Path::new("runtime").join(".meshscale-server.cjs")
        && manifest.runtime.args.is_empty();
    ensure!(
        manifest.runtime.working_directory == Path::new("runtime")
            && (legacy_cli || function_runtime || production_server)
            && manifest.r#static.directory == Path::new("static"),
        "unsupported manifest runtime/static layout"
    );
    for (name, value) in [
        ("id", &manifest.deployment.id),
        ("repository", &manifest.deployment.repository),
        ("commit", &manifest.deployment.commit),
        ("branch", &manifest.deployment.branch),
    ] {
        ensure!(
            !value.trim().is_empty(),
            "manifest deployment {name} cannot be empty"
        );
    }
    let runtime = resolve_path(output, &manifest.runtime.working_directory)?;
    let entrypoint = resolve_path(output, &manifest.runtime.entrypoint)?;
    let assets = if server_only && manifest.version == 2 && !output.join("static").exists() {
        None
    } else {
        Some(resolve_path(output, &manifest.r#static.directory)?)
    };
    ensure!(
        runtime.is_dir(),
        "manifest working directory is not a directory"
    );
    ensure!(
        entrypoint.is_file() && entrypoint.starts_with(&runtime),
        "manifest entrypoint is not a runtime file"
    );
    ensure!(
        assets.as_ref().is_none_or(|path| path.is_dir()),
        "manifest static directory is not a directory"
    );
    if function_runtime {
        let adapter = resolve_path(
            output,
            &Path::new("runtime").join(".next").join("meshscale-adapter.json"),
        )?;
        let adapter: serde_json::Value = serde_json::from_slice(&fs::read(adapter)?)
            .context("invalid MeshScale Next.js adapter metadata")?;
        ensure!(
            adapter.get("version").and_then(serde_json::Value::as_u64) == Some(1),
            "unsupported MeshScale adapter metadata version"
        );
    } else if production_server {
        let config = resolve_path(
            output,
            &Path::new("runtime")
                .join(".next")
                .join("required-server-files.json"),
        )?;
        let config: serde_json::Value = serde_json::from_slice(&fs::read(config)?)
            .context("invalid Next.js required-server-files.json")?;
        ensure!(
            config
                .get("config")
                .is_some_and(serde_json::Value::is_object),
            "Next.js required-server-files.json is missing its build configuration"
        );
    }
    if manifest.version == 2 {
        manifest.validate_v2()?;
    }
    Ok(manifest)
}

impl Manifest {
    pub fn validate_v2(&self) -> Result<()> {
        ensure!(
            self.deployment.org_id.is_none() || self.deployment.project_id.is_some(),
            "organization requires project ID"
        );
        for (name, id) in [
            ("org-id", &self.deployment.org_id),
            ("project-id", &self.deployment.project_id),
        ] {
            if let Some(id) = id {
                crate::upload::validate_id(name, id)?;
            }
        }
        ensure!(
            self.version == 2,
            "rebuild this legacy artifact for manifest v2 upload/package"
        );
        ensure!(
            self.runtime.entrypoint == Path::new("runtime").join("function-entry.cjs")
                && self.runtime.args.is_empty(),
            "v2 requires the MeshScale function entrypoint"
        );
        crate::upload::validate_id("build-id", &self.deployment.id)?;
        ensure!(
            self.deployment.commit.len() == 40
                && self
                    .deployment
                    .commit
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "v2 requires the resolved full 40-character Git commit"
        );
        ensure!(
            self.deployment
                .next_build_id
                .as_ref()
                .is_some_and(|id| !id.is_empty()),
            "missing Next build ID"
        );
        ensure!(
            self.deployment
                .next_version
                .as_ref()
                .is_some_and(|id| !id.is_empty()),
            "missing Next version"
        );
        let platform = self
            .platform
            .as_ref()
            .context("missing platform requirements")?;
        ensure!(
            !platform.os.is_empty()
                && !platform.arch.is_empty()
                && !platform.node_version.is_empty()
                && !platform.node_abi.is_empty(),
            "invalid platform requirements"
        );
        let target = self
            .server
            .as_ref()
            .context("missing logical server target")?;
        ensure!(
            target.target_id == self.target_id(),
            "server target does not match deployment"
        );
        for (key, object) in &self.r#static.objects {
            validate_relative(key)?;
            ensure!(
                object.sha256.len() == 64
                    && object
                        .sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "invalid static SHA-256 for {key}"
            );
            ensure!(object.status == 200, "invalid static response status");
            ensure!(
                matches!(
                    object.class.as_str(),
                    "public_asset" | "next_asset" | "prerender_html" | "prerender_data"
                ),
                "unsupported static object class"
            );
            for value in [
                &object.content_type,
                &object.cache_control,
                &object.last_modified,
            ] {
                ensure!(!value.is_empty(), "empty static response metadata");
                axum::http::HeaderValue::from_str(value)
                    .context("invalid static response metadata")?;
            }
            httpdate::parse_http_date(&object.last_modified)
                .context("invalid static Last-Modified")?;
            for (name, value) in &object.headers {
                ensure!(
                    matches!(
                        name.as_str(),
                        "content-language" | "content-disposition" | "x-content-type-options"
                    ),
                    "unsupported static response header {name}"
                );
                axum::http::HeaderValue::from_str(value)?;
            }
        }
        let routing = self.routing.as_ref().context("missing routing contract")?;
        routing.validate(&self.r#static.objects)?;
        if let Some(storage) = &self.r#static.storage {
            ensure!(
                storage.provider == "r2"
                    && !storage.bucket.is_empty()
                    && storage.prefix == format!("{}/static/", self.bound_prefix()?),
                "invalid R2 storage binding"
            );
        }
        Ok(())
    }

    pub fn target_id(&self) -> String {
        format!(
            "{}/{}/{}",
            self.deployment.org_id.as_deref().unwrap_or("_local"),
            self.deployment.project_id.as_deref().unwrap_or("_local"),
            self.deployment.id
        )
    }

    pub fn bound_prefix(&self) -> Result<String> {
        let org = self
            .deployment
            .org_id
            .as_deref()
            .context("missing organization ID")?;
        let project = self
            .deployment
            .project_id
            .as_deref()
            .context("missing project ID")?;
        crate::upload::validate_id("org-id", org)?;
        crate::upload::validate_id("project-id", project)?;
        Ok(format!("{org}/{project}/{}", self.deployment.id))
    }

    pub fn bind(&mut self, org: &str, project: &str, bucket: Option<&str>) -> Result<()> {
        for (stored, supplied) in [
            (&self.deployment.org_id, org),
            (&self.deployment.project_id, project),
        ] {
            ensure!(
                stored.as_ref().is_none_or(|stored| stored == supplied),
                "deployment identity mismatch"
            );
        }
        self.deployment.org_id = Some(org.to_owned());
        self.deployment.project_id = Some(project.to_owned());
        self.server = Some(Server {
            target_id: self.target_id(),
        });
        if let Some(bucket) = bucket {
            let storage = Storage {
                provider: "r2".to_owned(),
                bucket: bucket.to_owned(),
                prefix: format!("{}/static/", self.bound_prefix()?),
            };
            ensure!(
                self.r#static
                    .storage
                    .as_ref()
                    .is_none_or(|old| old.bucket == storage.bucket && old.prefix == storage.prefix),
                "artifact is already bound to another R2 location"
            );
            self.r#static.storage = Some(storage);
        }
        self.validate_v2()
    }
}

pub fn validate_relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains(['\\', ':', '\0'])
            && path.split('/').all(|part| !part.is_empty()
                && part != "."
                && part != ".."
                && part != ".env"
                && !part.starts_with(".env.")),
        "invalid artifact-relative path: {path}"
    );
    Ok(())
}

pub fn write(output: &Path, manifest: &Manifest) -> Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(output)?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), manifest)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(output.join("manifest.json"))
        .map_err(|error| error.error)
        .context("failed to publish local manifest")?;
    Ok(())
}

pub fn resolve_path(output: &Path, relative: &Path) -> Result<PathBuf> {
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "manifest path must be a nonempty relative path without traversal: {}",
        relative.display()
    );
    let root = output
        .canonicalize()
        .context("failed to resolve output directory")?;
    let path = root
        .join(relative)
        .canonicalize()
        .with_context(|| format!("manifest path does not exist: {}", relative.display()))?;
    ensure!(
        path.starts_with(&root),
        "manifest path escapes output: {}",
        relative.display()
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_manifest_paths() -> Result<()> {
        let root = tempfile::TempDir::new()?;
        for path in ["", "..", "runtime/../../outside"] {
            assert!(resolve_path(root.path(), Path::new(path)).is_err());
        }
        assert!(resolve_path(root.path(), &root.path().join("absolute")).is_err());
        Ok(())
    }
}
