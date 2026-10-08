use crate::{
    manifest::{Manifest, Platform, Server, StaticObject},
    routing::{QueryPolicy, Routing},
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::Read, path::Path, process::Command};
use walkdir::WalkDir;

pub fn digest_file(path: &Path) -> Result<(u64, String)> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut bytes = 0;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        bytes += count as u64;
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}

pub fn json(path: &Path) -> Result<Value> {
    serde_json::from_slice(
        &fs::read(path).with_context(|| format!("missing build metadata {}", path.display()))?,
    )
    .with_context(|| format!("invalid build metadata {}", path.display()))
}

pub fn object(path: &Path, class: &str) -> Result<StaticObject> {
    let (bytes, sha256) = digest_file(path)?;
    Ok(StaticObject {
        bytes,
        sha256,
        class: class.to_owned(),
        status: 200,
        headers: BTreeMap::new(),
        content_type: mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string(),
        cache_control: if class == "next_asset" {
            "public, max-age=31536000, immutable"
        } else {
            "public, max-age=0, must-revalidate"
        }
        .into(),
        last_modified: httpdate::fmt_http_date(fs::metadata(path)?.modified()?),
    })
}

pub fn verify_inventory(output: &Path, manifest: &Manifest) -> Result<()> {
    manifest.validate_v2()?;
    let root = output.join("static");
    let mut found = BTreeMap::new();
    for entry in WalkDir::new(&root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let key = entry
            .path()
            .strip_prefix(&root)?
            .to_str()
            .context("static path is not UTF-8")?
            .replace('\\', "/");
        let declared = manifest
            .r#static
            .objects
            .get(&key)
            .with_context(|| format!("unlisted static object {key}"))?;
        let (bytes, digest) = digest_file(entry.path())?;
        ensure!(
            bytes == declared.bytes && digest == declared.sha256,
            "static object does not match manifest: {key}"
        );
        found.insert(key, ());
    }
    ensure!(
        found.len() == manifest.r#static.objects.len(),
        "manifest static objects are missing"
    );
    Ok(())
}

pub fn generate(project: &Path, output: &Path, manifest: &mut Manifest) -> Result<()> {
    let next = project.join(".next");
    let routes = json(&next.join("routes-manifest.json"))?;
    let prerender = json(&next.join("prerender-manifest.json"))?;
    let middleware = json(&next.join("server").join("middleware-manifest.json"))?;
    ensure!(
        routes["version"] == 3 && prerender["version"] == 4 && middleware["version"] == 3,
        "unsupported Next.js routing metadata versions; refusing to guess edge routing"
    );
    let config = json(&next.join("required-server-files.json"))?["config"].clone();
    let next_package = json(
        &project
            .join("node_modules")
            .join("next")
            .join("package.json"),
    )?;
    let next_version = next_package["version"]
        .as_str()
        .context("missing Next version")?;
    ensure!(
        ["14.", "15.", "16."]
            .iter()
            .any(|major| next_version.starts_with(major)),
        "unsupported Next.js version for routing classification: {next_version}"
    );
    manifest.deployment.next_version = Some(next_version.into());
    manifest.deployment.next_build_id =
        Some(fs::read_to_string(next.join("BUILD_ID"))?.trim().into());
    let mut command = Command::new("node");
    crate::upload::remove_credentials(&mut command);
    let node = command
        .args([
            "-e",
            "console.log(JSON.stringify({version:process.version,abi:process.versions.modules}))",
        ])
        .output()
        .context("failed to inspect build Node runtime")?;
    ensure!(
        node.status.success(),
        "failed to inspect build Node runtime"
    );
    let node: Value = serde_json::from_slice(&node.stdout)?;
    manifest.platform = Some(Platform {
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        node_version: node["version"]
            .as_str()
            .context("missing Node version")?
            .into(),
        node_abi: node["abi"].as_str().context("missing Node ABI")?.into(),
    });
    manifest.server = Some(Server {
        target_id: manifest.target_id(),
    });
    let mut routing = Routing::new();
    let mut reasons = Vec::new();
    if middleware["middleware"]
        .as_object()
        .is_none_or(|items| !items.is_empty())
        || middleware["functions"]
            .as_object()
            .is_none_or(|items| !items.is_empty())
        || config
            .get("experimental")
            .and_then(|v| v.get("ppr"))
            .is_some_and(|v| v != &Value::Bool(false))
        || config["cacheComponents"] == true
        || config["experimental"]["dynamicIO"] == true
    {
        reasons.push("middleware-proxy-or-partial-prerendering".to_owned());
    }
    if config.get("i18n").is_some_and(|v| !v.is_null()) {
        reasons.push("locale-routing".to_owned());
    }
    if routes["headers"]
        .as_array()
        .is_none_or(|items| !items.is_empty())
        || routes
            .get("onMatchHeaders")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
        || routes["redirects"]
            .as_array()
            .is_none_or(|items| items.iter().any(|item| item["internal"] != true))
        || !empty_rewrites(&routes["rewrites"])
    {
        reasons.push("custom-routing-precedence".to_owned());
    }
    let base = config["basePath"].as_str().unwrap_or("");
    ensure!(
        base.is_empty() || (base.starts_with('/') && !base.ends_with('/')),
        "invalid Next basePath"
    );
    if config["assetPrefix"]
        .as_str()
        .is_some_and(|prefix| !prefix.is_empty() && prefix != base)
    {
        reasons.push("custom-asset-prefix".to_owned());
    }
    let static_dir = output.join("static");
    for entry in WalkDir::new(&static_dir).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let key = entry
            .path()
            .strip_prefix(&static_dir)?
            .to_str()
            .context("static file path is not UTF-8")?
            .replace('\\', "/");
        crate::manifest::validate_relative(&key)?;
        ensure!(
            !key.starts_with("_prerender/"),
            "public assets conflict with reserved _prerender namespace"
        );
        let class = if key.starts_with("_next/static/") {
            "next_asset"
        } else {
            "public_asset"
        };
        manifest
            .r#static
            .objects
            .insert(key.clone(), object(entry.path(), class)?);
        if reasons.is_empty() {
            routing.insert_static(format!("{base}/{key}"), key, QueryPolicy::Ignore)?;
        }
    }
    let pages = if next.join("server").join("pages-manifest.json").is_file() {
        json(&next.join("server").join("pages-manifest.json"))?
    } else {
        Value::Null
    };
    let app_paths = if next
        .join("server")
        .join("app-paths-manifest.json")
        .is_file()
    {
        json(&next.join("server").join("app-paths-manifest.json"))?
    } else {
        Value::Null
    };
    let app_routes = if next.join("app-path-routes-manifest.json").is_file() {
        json(&next.join("app-path-routes-manifest.json"))?
    } else {
        Value::Null
    };
    for (url, info) in prerender["routes"]
        .as_object()
        .context("prerender manifest has no route map")?
    {
        if !reasons.is_empty()
            || (url == "/" && !base.is_empty())
            || !eligible(info)
            || url.starts_with("/_")
            || info["initialStatus"]
                .as_u64()
                .is_some_and(|status| status != 200)
        {
            routing
                .fallback_reasons
                .push(format!("{url}:runtime-or-unsupported-prerender"));
            continue;
        }
        // A matching dynamic template with runtime fallback remains server-owned.
        let template = info["srcRoute"].as_str().unwrap_or(url);
        if prerender["dynamicRoutes"]
            .get(template)
            .is_some_and(|dynamic| {
                dynamic
                    .get("fallback")
                    .is_none_or(|value| value != &Value::Bool(false))
            })
        {
            routing
                .fallback_reasons
                .push(format!("{url}:dynamic-fallback"));
            continue;
        }
        let page_source = pages
            .get(url)
            .or_else(|| pages.get(template))
            .and_then(Value::as_str);
        let app_source = app_routes
            .as_object()
            .and_then(|items| {
                items.iter().find(|(_, route)| {
                    route.as_str() == Some(url) || route.as_str() == Some(template)
                })
            })
            .and_then(|(key, _)| app_paths.get(key))
            .and_then(Value::as_str);
        let source = page_source
            .filter(|path| path.ends_with(".html"))
            .map(str::to_owned)
            .or_else(|| {
                page_source.filter(|path| path.ends_with(".js")).map(|_| {
                    if url == "/" {
                        "pages/index.html".into()
                    } else {
                        format!("pages{url}.html")
                    }
                })
            })
            .or_else(|| {
                app_source
                    .filter(|path| path.ends_with("/page.js"))
                    .map(|path| {
                        if template.contains('[') {
                            format!("app{url}.html")
                        } else if path == "app/page.js" {
                            "app/index.html".into()
                        } else {
                            format!("{}.html", path.trim_end_matches("/page.js"))
                        }
                    })
            });
        let Some(source) = source else {
            routing
                .fallback_reasons
                .push(format!("{url}:unsupported-response-layout"));
            continue;
        };
        crate::manifest::validate_relative(&source)?;
        let source_path = next.join("server").join(&source);
        if !source_path.is_file() {
            routing.fallback_reasons.push(format!("{url}:missing-html"));
            continue;
        }
        // Unknown/custom response headers can influence routing or personalization.
        if info
            .get("initialHeaders")
            .and_then(Value::as_object)
            .is_some_and(|headers| {
                headers
                    .iter()
                    .any(|(key, value)| match key.to_ascii_lowercase().as_str() {
                        "x-next-cache-tags" => false,
                        "content-type" => value.as_str().is_none_or(|value| {
                            !value.to_ascii_lowercase().starts_with("text/html")
                        }),
                        "cache-control" => value.as_str().is_none_or(|value| {
                            value.to_ascii_lowercase().split(',').any(|directive| {
                                matches!(
                                    directive.trim().split('=').next().unwrap_or(""),
                                    "private" | "no-store" | "no-cache"
                                )
                            })
                        }),
                        _ => true,
                    })
            })
        {
            routing
                .fallback_reasons
                .push(format!("{url}:unsupported-response-headers"));
            continue;
        }
        let key = format!("_prerender/{source}");
        stage(&source_path, &static_dir, &key, "prerender_html", manifest)?;
        let path = if config["trailingSlash"] == true && url != "/" {
            format!("{base}{}/", url.trim_end_matches('/'))
        } else {
            format!("{base}{url}")
        };
        routing.insert_static(path, key, QueryPolicy::Empty)?;
        if page_source.is_some()
            && let Some(data_url) = info["dataRoute"]
                .as_str()
                .filter(|path| path.ends_with(".json"))
        {
            let data_source = next
                .join("server")
                .join(source.trim_end_matches(".html").to_owned() + ".json");
            if data_source.is_file() {
                let key = format!("_prerender/{}.json", source.trim_end_matches(".html"));
                stage(&data_source, &static_dir, &key, "prerender_data", manifest)?;
                routing.insert_static(format!("{base}{data_url}"), key, QueryPolicy::Empty)?;
            }
        }
    }
    routing.fallback_reasons.extend(reasons);
    manifest.routing = Some(routing);
    manifest.validate_v2()
}

fn stage(
    source: &Path,
    root: &Path,
    key: &str,
    class: &str,
    manifest: &mut Manifest,
) -> Result<()> {
    let destination = root.join(key);
    ensure!(!destination.exists(), "static staging collision: {key}");
    fs::create_dir_all(destination.parent().context("staging path has no parent")?)?;
    fs::copy(source, &destination)?;
    let mut meta = object(&destination, class)?;
    meta.content_type = if class == "prerender_html" {
        "text/html; charset=utf-8"
    } else {
        "application/json"
    }
    .into();
    manifest.r#static.objects.insert(key.into(), meta);
    Ok(())
}

fn empty_rewrites(value: &Value) -> bool {
    value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(|items| {
            ["beforeFiles", "afterFiles", "fallback"].iter().all(|key| {
                items
                    .get(*key)
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
            })
        })
}

fn eligible(info: &Value) -> bool {
    info["initialRevalidateSeconds"] == false
        && info.get("response").is_none_or(|value| value == "complete")
        && info.get("compute").is_none_or(|value| value == "static")
        && info.get("routeType").is_none_or(|value| value == "page")
        && !info
            .get("experimentalPPR")
            .is_some_and(|value| value != &Value::Bool(false))
        && !info
            .get("postponed")
            .is_some_and(|value| !value.is_null() && value != &Value::Bool(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(path: &Path, value: Value) -> Result<()> {
        fs::create_dir_all(path.parent().context("test metadata has no parent")?)?;
        fs::write(path, value.to_string())?;
        Ok(())
    }

    fn fixture() -> Result<(tempfile::TempDir, Manifest)> {
        let root = tempfile::TempDir::new()?;
        let project = root.path().join("repo");
        let next = project.join(".next");
        fs::create_dir_all(root.path().join("output").join("static"))?;
        fs::write(
            root.path().join("output").join("static").join("asset.txt"),
            "asset",
        )?;
        fs::create_dir_all(next.join("server").join("pages").join("blog"))?;
        fs::create_dir_all(next.join("server").join("app"))?;
        fs::write(next.join("BUILD_ID"), "next-build")?;
        fs::write(
            next.join("server").join("pages").join("about.html"),
            "about",
        )?;
        fs::write(next.join("server").join("pages").join("about.json"), "{}")?;
        fs::write(
            next.join("server")
                .join("pages")
                .join("blog")
                .join("a.html"),
            "dynamic prerender",
        )?;
        fs::write(
            next.join("server").join("app").join("contact.html"),
            "contact",
        )?;
        write(
            &next.join("required-server-files.json"),
            serde_json::json!({"config":{"basePath":"","trailingSlash":false}}),
        )?;
        write(
            &project
                .join("node_modules")
                .join("next")
                .join("package.json"),
            serde_json::json!({"version":"16.3.4"}),
        )?;
        write(
            &next.join("routes-manifest.json"),
            serde_json::json!({"version":3,"headers":[],"redirects":[],
            "rewrites":{"beforeFiles":[],"afterFiles":[],"fallback":[]}}),
        )?;
        write(
            &next.join("server").join("middleware-manifest.json"),
            serde_json::json!({"version":3,"middleware":{},"functions":{}}),
        )?;
        write(
            &next.join("server").join("pages-manifest.json"),
            serde_json::json!({"/about":"pages/about.html","/blog/[slug]":"pages/blog/[slug].js"}),
        )?;
        write(
            &next.join("server").join("app-paths-manifest.json"),
            serde_json::json!({"/contact/page":"app/contact/page.js"}),
        )?;
        write(
            &next.join("app-path-routes-manifest.json"),
            serde_json::json!({"/contact/page":"/contact"}),
        )?;
        write(
            &next.join("prerender-manifest.json"),
            serde_json::json!({"version":4,"routes":{
            "/about":{"initialRevalidateSeconds":false,"dataRoute":"/_next/data/next-build/about.json"},
            "/contact":{"initialRevalidateSeconds":false,"compute":"static","response":"complete","routeType":"page","dataRoute":"/contact.rsc"},
            "/blog/a":{"initialRevalidateSeconds":false,"srcRoute":"/blog/[slug]"},
            "/isr":{"initialRevalidateSeconds":60},
            "/partial":{"initialRevalidateSeconds":false,"response":"partial"},
            "/metadata":{"initialRevalidateSeconds":false,"routeType":"route"}
        },"dynamicRoutes":{"/blog/[slug]":{"fallback":false}}, "preview":{"previewModeSigningKey":"never-publish-secret"}}),
        )?;
        let metadata = serde_json::from_value(serde_json::json!({
            "version":2,"framework":"nextjs",
            "runtime":{"type":"node","command":"node","entrypoint":"runtime/function-entry.cjs","working_directory":"runtime","args":[]},
            "static":{"directory":"static"},
            "deployment":{"id":"build_1","repository":"test/repo","commit":"1234567890abcdef1234567890abcdef12345678","branch":"main"}
        }))?;
        Ok((root, metadata))
    }

    #[test]
    fn derives_exact_pages_app_html_and_pages_data_without_leaking_preview() -> Result<()> {
        let (root, mut metadata) = fixture()?;
        generate(
            &root.path().join("repo"),
            &root.path().join("output"),
            &mut metadata,
        )?;
        let routing = metadata.routing.as_ref().unwrap();
        let headers = axum::http::HeaderMap::new();
        for path in [
            "/about",
            "/contact",
            "/blog/a",
            "/_next/data/next-build/about.json",
            "/asset.txt",
        ] {
            assert!(
                routing
                    .select(&axum::http::Method::GET, &path.parse()?, &headers)?
                    .is_some(),
                "{path}"
            );
        }
        for path in [
            "/isr",
            "/partial",
            "/metadata",
            "/contact.rsc",
            "/blog/unknown",
            "/_prerender/pages/about.html",
        ] {
            assert!(
                routing
                    .select(&axum::http::Method::GET, &path.parse()?, &headers)?
                    .is_none(),
                "{path}"
            );
        }
        assert!(!serde_json::to_string(&metadata)?.contains("never-publish-secret"));
        verify_inventory(&root.path().join("output"), &metadata)?;
        let schema: Value = serde_json::from_str(&fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("manifest.schema.json"),
        )?)?;
        let validator = jsonschema::validator_for(&schema)?;
        let value = serde_json::to_value(&metadata)?;
        assert!(
            validator.is_valid(&value),
            "generated manifest does not match schema"
        );
        Ok(())
    }

    #[test]
    fn forwards_custom_routing_middleware_and_locales_conservatively() -> Result<()> {
        for kind in [
            "middleware",
            "rewrites",
            "headers",
            "redirects",
            "locale",
            "ppr",
            "cache_components",
            "asset_prefix",
            "dynamic_io",
        ] {
            let (root, mut metadata) = fixture()?;
            let next = root.path().join("repo").join(".next");
            match kind {
                "middleware" => write(
                    &next.join("server").join("middleware-manifest.json"),
                    serde_json::json!({"version":3,"middleware":{"/":{}},"functions":{}}),
                )?,
                "locale" | "ppr" | "cache_components" | "asset_prefix" | "dynamic_io" => write(
                    &next.join("required-server-files.json"),
                    match kind {
                        "locale" => serde_json::json!({"config":{"i18n":{"locales":["en"]}}}),
                        "ppr" => serde_json::json!({"config":{"experimental":{"ppr":true}}}),
                        "cache_components" => {
                            serde_json::json!({"config":{"cacheComponents":true}})
                        }
                        "dynamic_io" => {
                            serde_json::json!({"config":{"experimental":{"dynamicIO":true}}})
                        }
                        _ => {
                            serde_json::json!({"config":{"assetPrefix":"https://cdn.example.test"}})
                        }
                    },
                )?,
                _ => {
                    let mut routes = json(&next.join("routes-manifest.json"))?;
                    routes[kind] = if kind == "rewrites" {
                        serde_json::json!({"beforeFiles":[{}],"afterFiles":[],"fallback":[]})
                    } else {
                        serde_json::json!([{}])
                    };
                    write(&next.join("routes-manifest.json"), routes)?;
                }
            }
            generate(
                &root.path().join("repo"),
                &root.path().join("output"),
                &mut metadata,
            )?;
            let routing = metadata.routing.unwrap();
            assert_eq!(routing.rules.len(), 2, "{kind}");
            assert!(!routing.fallback_reasons.is_empty());
        }
        Ok(())
    }

    #[test]
    fn respects_base_path_trailing_slash_and_rejects_unknown_versions() -> Result<()> {
        let (root, mut metadata) = fixture()?;
        let next = root.path().join("repo").join(".next");
        write(
            &next.join("required-server-files.json"),
            serde_json::json!({"config":{"basePath":"/docs","trailingSlash":true}}),
        )?;
        generate(
            &root.path().join("repo"),
            &root.path().join("output"),
            &mut metadata,
        )?;
        let routing = metadata.routing.as_ref().unwrap();
        let headers = axum::http::HeaderMap::new();
        assert!(
            routing
                .select(&axum::http::Method::GET, &"/docs/about/".parse()?, &headers)?
                .is_some()
        );
        assert!(
            routing
                .select(&axum::http::Method::GET, &"/docs/about".parse()?, &headers)?
                .is_none()
        );
        assert!(
            routing
                .select(
                    &axum::http::Method::GET,
                    &"/docs/asset.txt".parse()?,
                    &headers
                )?
                .is_some()
        );
        write(
            &next.join("routes-manifest.json"),
            serde_json::json!({"version":99}),
        )?;
        assert!(
            generate(
                &root.path().join("repo"),
                &root.path().join("output"),
                &mut metadata
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn refuses_isr_partial_and_runtime_responses() {
        assert!(eligible(
            &serde_json::json!({"initialRevalidateSeconds": false, "compute":"static","response":"complete"})
        ));
        for info in [
            serde_json::json!({"initialRevalidateSeconds":60}),
            serde_json::json!({"initialRevalidateSeconds":false,"response":"partial"}),
            serde_json::json!({"initialRevalidateSeconds":false,"compute":"dynamic"}),
            serde_json::json!({"initialRevalidateSeconds":false,"routeType":"route"}),
            serde_json::json!({"initialRevalidateSeconds":false,"postponed":"state"}),
        ] {
            assert!(!eligible(&info));
        }
        assert!(empty_rewrites(
            &serde_json::json!({"beforeFiles":[],"afterFiles":[],"fallback":[]})
        ));
        assert!(!empty_rewrites(
            &serde_json::json!({"beforeFiles":[{}],"afterFiles":[],"fallback":[]})
        ));
    }
}
