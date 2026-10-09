use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};
use walkdir::WalkDir;
use zip::{CompressionMethod, System, ZipArchive, ZipWriter, write::SimpleFileOptions};

use crate::{artifact, manifest};

/// Lambda handler string for the generated `runtime/lambda-entry.cjs`.
pub const HANDLER: &str = "lambda-entry.handler";

/// Lambda Node.js runtime identifiers accepted by `--node-runtime`. Checked on 2026-10-09
/// against https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtimes.html: `nodejs20.x`
/// was deprecated on 2026-04-30 and `nodejs26.x` is a preview (not for production). All of
/// these run on Amazon Linux 2023.
pub const SUPPORTED_NODE_RUNTIMES: [&str; 2] = ["nodejs22.x", "nodejs24.x"];

/// Lambda deployment package limits. AWS documents "MB" as 1,024 KB. Checked on 2026-10-09
/// against https://docs.aws.amazon.com/lambda/latest/dg/gettingstarted-limits.html.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// Contents of the package, including layers, unzipped (250 MB).
    max_unzipped_bytes: u64,
    /// Largest .zip accepted by a direct Lambda API upload (50 MB); larger ones go through S3.
    max_direct_upload_bytes: u64,
}

const LIMITS: Limits = Limits {
    max_unzipped_bytes: 250 * 1024 * 1024,
    max_direct_upload_bytes: 50 * 1024 * 1024,
};

/// Generated files the Lambda handler needs.
const REQUIRED_FILES: [&str; 3] = ["lambda-entry.cjs", "lambda-adapter.cjs", "runtime.cjs"];
/// Top-level runtime files left out of the package (the IPC shell is not used on Lambda).
const EXCLUDED_FILES: [&str; 1] = ["function-entry.cjs"];
const FILE_MODE: u32 = 0o644;
const DIRECTORY_MODE: u32 = 0o755;
const COMPRESSION_LEVEL: i64 = 6;
const REPORT_ENTRIES: usize = 20;
const SIDECAR_SCHEMA_VERSION: u32 = 1;

#[derive(Args, Debug)]
pub struct PackageArgs {
    #[command(subcommand)]
    pub target: PackageTarget,
}

#[derive(Subcommand, Debug)]
pub enum PackageTarget {
    /// Package runtime/ of a built output as an AWS Lambda .zip plus a lambda.json sidecar.
    Lambda(LambdaArgs),
}

#[derive(Args, Debug)]
pub struct LambdaArgs {
    /// Path to a built .meshscale/output directory.
    pub output: PathBuf,
    /// Lambda architecture. Must match the CPU architecture the output was built on.
    #[arg(long, value_enum)]
    pub arch: Architecture,
    /// Lambda Node.js runtime. Its major version must match the Node.js that built the output.
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(SUPPORTED_NODE_RUNTIMES))]
    pub node_runtime: String,
    /// Destination directory for lambda.zip and lambda.json (default: <OUTPUT>/lambda).
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
pub enum Architecture {
    #[value(name = "x86_64")]
    #[serde(rename = "x86_64")]
    X86_64,
    #[value(name = "arm64")]
    #[serde(rename = "arm64")]
    Arm64,
}

impl Architecture {
    /// Maps `manifest.platform.arch` (Rust's `std::env::consts::ARCH`) to a Lambda architecture.
    fn from_platform(arch: &str) -> Option<Self> {
        match arch {
            "x86_64" => Some(Self::X86_64),
            "aarch64" => Some(Self::Arm64),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Arm64 => "arm64",
        }
    }
}

/// `lambda.json`, written next to `lambda.zip`.
#[derive(Debug, Serialize)]
pub struct Sidecar {
    pub schema_version: u32,
    pub build_id: String,
    pub commit: String,
    pub handler: &'static str,
    pub runtime: String,
    pub architecture: Architecture,
    pub node_major: u64,
    pub zip_sha256: String,
    pub zip_bytes: u64,
    pub uncompressed_bytes: u64,
    pub file_count: usize,
    /// Byte-for-byte reproducibility holds for one builder version.
    pub builder_version: &'static str,
    /// The zip is above the direct-upload limit and must be deployed from S3.
    pub requires_s3_upload: bool,
    pub report: SizeReport,
}

#[derive(Debug, Serialize)]
pub struct SizeReport {
    pub largest_files: Vec<FileSize>,
    pub largest_packages: Vec<PackageSize>,
    /// Total size of `*.map` files, to decide whether a Lambda-specific prune is worthwhile.
    pub source_map_bytes: u64,
    /// Total size under `.next/static/` (also served from static/ by the edge).
    pub next_static_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct FileSize {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct PackageSize {
    pub name: String,
    pub bytes: u64,
    pub files: usize,
}

#[derive(Debug, Serialize)]
pub struct PackageResult {
    pub zip: PathBuf,
    pub sidecar: PathBuf,
    #[serde(flatten)]
    pub lambda: Sidecar,
}

enum Entry {
    File { source: PathBuf, bytes: u64 },
    Directory,
}

pub fn package(args: PackageArgs) -> Result<()> {
    match args.target {
        PackageTarget::Lambda(args) => {
            let result = package_lambda(&args, LIMITS)?;
            if result.lambda.requires_s3_upload {
                eprintln!(
                    "lambda.zip is {} bytes, above the {} byte direct-upload limit: deploy it from S3",
                    result.lambda.zip_bytes, LIMITS.max_direct_upload_bytes
                );
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "success", "package": result
                }))?
            );
            Ok(())
        }
    }
}

fn package_lambda(args: &LambdaArgs, limits: Limits) -> Result<PackageResult> {
    // 1. The output validates and has a version-2 manifest.
    artifact::validate_output(&args.output)?;
    let manifest = manifest::load(&args.output)?;
    manifest
        .validate_v2()
        .context("package lambda needs a version-2 output; rebuild it")?;
    let runtime = manifest::resolve_path(&args.output, &manifest.runtime.working_directory)?;

    // 2. The generated Lambda files exist.
    for name in REQUIRED_FILES {
        ensure!(
            runtime.join(name).is_file(),
            "runtime/{name} is missing in {}; rebuild the output with a Lambda-capable builder",
            args.output.display()
        );
    }

    // 3. Platform: Linux, same architecture, same Node.js major (same major implies same ABI).
    let platform = manifest
        .platform
        .as_ref()
        .context("manifest.json has no platform; rebuild the output")?;
    ensure!(
        platform.os == "linux",
        "the output was built on {}; Lambda needs an output built on Linux (glibc, not musl/Alpine) on the target architecture. Cross-building native packages is not supported",
        platform.os
    );
    let built_for = Architecture::from_platform(&platform.arch).with_context(|| {
        format!(
            "the output was built on unsupported architecture {}",
            platform.arch
        )
    })?;
    ensure!(
        built_for == args.arch,
        "the output was built on {} (Lambda {}) but --arch is {}; build on the target architecture",
        platform.arch,
        built_for.as_str(),
        args.arch.as_str()
    );
    let node_major = node_major(&platform.node_version)?;
    let runtime_major = runtime_major(&args.node_runtime)?;
    ensure!(
        node_major == runtime_major,
        "the output was built with Node.js {} but --node-runtime is {}; native modules need the same Node.js major",
        platform.node_version,
        args.node_runtime
    );

    // 4. Package contents: no links, no `..`, no backslashes, no .env files.
    let entries = collect_entries(&runtime)?;

    // 5. Next.js has the stable Adapter API.
    let next_version = manifest
        .deployment
        .next_version
        .as_deref()
        .context("manifest.json does not record the Next.js version; rebuild the output")?;
    crate::build::ensure_supported_next(next_version)?;

    let out = args
        .out
        .clone()
        .unwrap_or_else(|| args.output.join("lambda"));
    fs::create_dir_all(&out).with_context(|| format!("failed to create {}", out.display()))?;
    ensure!(
        !out.canonicalize()?.starts_with(runtime.canonicalize()?),
        "--out {} is inside runtime/, which is what is being packaged",
        out.display()
    );

    let uncompressed_bytes = entries
        .values()
        .map(|entry| match entry {
            Entry::File { bytes, .. } => *bytes,
            Entry::Directory => 0,
        })
        .sum::<u64>();
    let file_count = entries
        .values()
        .filter(|entry| matches!(entry, Entry::File { .. }))
        .count();
    let report = size_report(&entries);
    if uncompressed_bytes > limits.max_unzipped_bytes {
        bail!(
            "runtime/ is {uncompressed_bytes} bytes unzipped, above the {} byte Lambda limit (which includes layers); nothing was written.\n{}",
            limits.max_unzipped_bytes,
            format_report(&report)
        );
    }

    let mut zip = tempfile::NamedTempFile::new_in(&out)
        .with_context(|| format!("failed to create a temporary zip in {}", out.display()))?;
    write_zip(&entries, zip.as_file_mut())?;
    zip.as_file().sync_all()?;
    verify_zip(zip.path(), &entries)?;
    let (zip_sha256, zip_bytes) = digest(zip.path())?;
    let zip_path = out.join("lambda.zip");
    zip.persist(&zip_path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to write {}", zip_path.display()))?;

    let sidecar = Sidecar {
        schema_version: SIDECAR_SCHEMA_VERSION,
        build_id: manifest.deployment.id.clone(),
        commit: manifest.deployment.commit.clone(),
        handler: HANDLER,
        runtime: args.node_runtime.clone(),
        architecture: args.arch,
        node_major,
        zip_sha256,
        zip_bytes,
        uncompressed_bytes,
        file_count,
        builder_version: env!("CARGO_PKG_VERSION"),
        requires_s3_upload: zip_bytes > limits.max_direct_upload_bytes,
        report,
    };
    let sidecar_path = out.join("lambda.json");
    let mut json = tempfile::NamedTempFile::new_in(&out)?;
    serde_json::to_writer_pretty(json.as_file_mut(), &sidecar)?;
    json.as_file_mut().write_all(b"\n")?;
    json.as_file().sync_all()?;
    json.persist(&sidecar_path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to write {}", sidecar_path.display()))?;

    Ok(PackageResult {
        zip: zip_path,
        sidecar: sidecar_path,
        lambda: sidecar,
    })
}

fn node_major(version: &str) -> Result<u64> {
    version
        .strip_prefix('v')
        .unwrap_or(version)
        .split('.')
        .next()
        .and_then(|major| major.parse().ok())
        .with_context(|| format!("manifest.json has an invalid Node.js version {version:?}"))
}

fn runtime_major(runtime: &str) -> Result<u64> {
    runtime
        .strip_prefix("nodejs")
        .and_then(|rest| rest.strip_suffix(".x"))
        .and_then(|major| major.parse().ok())
        .with_context(|| format!("invalid Lambda Node.js runtime {runtime:?}"))
}

/// Collects the package entries, keyed by their zip name and sorted bytewise. Directory
/// entries (with a trailing `/`) are kept only for empty directories.
fn collect_entries(runtime: &Path) -> Result<BTreeMap<String, Entry>> {
    let mut files = BTreeMap::new();
    let mut directories = Vec::new();
    for entry in WalkDir::new(runtime).follow_links(false) {
        let entry =
            entry.with_context(|| format!("failed to walk runtime {}", runtime.display()))?;
        let relative = entry.path().strip_prefix(runtime)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let mut parts = Vec::new();
        for component in relative.components() {
            let std::path::Component::Normal(part) = component else {
                bail!(
                    "runtime path {} is not a plain relative path",
                    relative.display()
                );
            };
            let part = part
                .to_str()
                .with_context(|| format!("runtime path {} is not UTF-8", relative.display()))?;
            ensure!(
                !part.contains('\\') && part != "..",
                "runtime path {} contains a backslash or '..'",
                relative.display()
            );
            ensure!(
                part != ".env" && !part.starts_with(".env."),
                "runtime contains a dotenv file: {}; dotenv files must never be packaged",
                relative.display()
            );
            parts.push(part);
        }
        let name = parts.join("/");
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink() && fs::read_link(entry.path()).is_err(),
            "runtime contains a link: {name}"
        );
        if metadata.is_dir() {
            directories.push(name);
        } else if metadata.is_file() {
            if EXCLUDED_FILES.contains(&name.as_str()) {
                continue;
            }
            files.insert(
                name,
                Entry::File {
                    source: entry.path().to_path_buf(),
                    bytes: metadata.len(),
                },
            );
        } else {
            bail!("runtime contains an unsupported file type: {name}");
        }
    }
    let mut entries = BTreeMap::new();
    for directory in &directories {
        let prefix = format!("{directory}/");
        let has_file = files
            .range(prefix.clone()..)
            .next()
            .is_some_and(|(name, _)| name.starts_with(&prefix));
        let has_directory = directories.iter().any(|other| other.starts_with(&prefix));
        if !has_file && !has_directory {
            entries.insert(prefix, Entry::Directory);
        }
    }
    entries.extend(files);
    Ok(entries)
}

fn options() -> SimpleFileOptions {
    // DEFAULT (not default()) fixes the modification time at 1980-01-01 00:00:00. Unix as the
    // host system keeps the modes; Windows would otherwise write DOS attributes.
    SimpleFileOptions::DEFAULT
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(COMPRESSION_LEVEL))
        .system(System::Unix)
}

fn write_zip(entries: &BTreeMap<String, Entry>, file: &mut fs::File) -> Result<()> {
    let mut writer = ZipWriter::new(file);
    for (name, entry) in entries {
        match entry {
            Entry::Directory => writer
                .add_directory(name.as_str(), options().unix_permissions(DIRECTORY_MODE))
                .with_context(|| format!("failed to add directory {name} to lambda.zip"))?,
            Entry::File { source, .. } => {
                writer
                    .start_file(name.as_str(), options().unix_permissions(FILE_MODE))
                    .with_context(|| format!("failed to add {name} to lambda.zip"))?;
                let mut input = fs::File::open(source)
                    .with_context(|| format!("failed to read {}", source.display()))?;
                io::copy(&mut input, &mut writer)
                    .with_context(|| format!("failed to compress {name}"))?;
            }
        }
    }
    writer.finish().context("failed to finish lambda.zip")?;
    Ok(())
}

/// Re-reads the written zip and compares it with the entries that were meant to be written.
fn verify_zip(path: &Path, entries: &BTreeMap<String, Entry>) -> Result<()> {
    let mut archive =
        ZipArchive::new(fs::File::open(path)?).context("failed to reopen lambda.zip")?;
    ensure!(
        archive.len() == entries.len(),
        "lambda.zip has {} entries, expected {}",
        archive.len(),
        entries.len()
    );
    for (index, (name, entry)) in entries.iter().enumerate() {
        let file = archive.by_index(index)?;
        let actual = file.name();
        ensure!(
            actual == name,
            "lambda.zip entry {index} is {actual}, expected {name}"
        );
        ensure!(
            !actual.starts_with('/')
                && !actual.contains('\\')
                && !actual.split('/').any(|part| part == "..")
                && file.enclosed_name().is_some(),
            "lambda.zip contains an unsafe entry name {actual}"
        );
        match entry {
            Entry::File { bytes, .. } => ensure!(
                !file.is_dir() && file.size() == *bytes,
                "lambda.zip entry {name} has {} bytes, expected {bytes}",
                file.size()
            ),
            Entry::Directory => {
                ensure!(file.is_dir(), "lambda.zip entry {name} is not a directory")
            }
        }
    }
    Ok(())
}

fn digest(path: &Path) -> Result<(String, u64)> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let bytes = io::copy(&mut file, &mut hasher)?;
    let hash = hasher
        .finalize()
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    Ok((hash, bytes))
}

fn size_report(entries: &BTreeMap<String, Entry>) -> SizeReport {
    let files = entries
        .iter()
        .filter_map(|(name, entry)| match entry {
            Entry::File { bytes, .. } => Some((name.as_str(), *bytes)),
            Entry::Directory => None,
        })
        .collect::<Vec<_>>();

    let mut largest_files = files
        .iter()
        .map(|(path, bytes)| FileSize {
            path: (*path).to_owned(),
            bytes: *bytes,
        })
        .collect::<Vec<_>>();
    largest_files.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    largest_files.truncate(REPORT_ENTRIES);

    let mut packages = BTreeMap::<String, (u64, usize)>::new();
    for (path, bytes) in &files {
        let mut parts = path.split('/');
        if parts.next() != Some("node_modules") {
            continue;
        }
        let Some(first) = parts.next() else { continue };
        let name = match (first.starts_with('@'), parts.next()) {
            (true, Some(second)) => format!("{first}/{second}"),
            _ => first.to_owned(),
        };
        let package = packages.entry(name).or_default();
        package.0 += bytes;
        package.1 += 1;
    }
    let mut largest_packages = packages
        .into_iter()
        .map(|(name, (bytes, files))| PackageSize { name, bytes, files })
        .collect::<Vec<_>>();
    largest_packages.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    largest_packages.truncate(REPORT_ENTRIES);

    SizeReport {
        largest_files,
        largest_packages,
        source_map_bytes: files
            .iter()
            .filter(|(path, _)| path.ends_with(".map"))
            .map(|(_, bytes)| bytes)
            .sum(),
        next_static_bytes: files
            .iter()
            .filter(|(path, _)| path.starts_with(".next/static/"))
            .map(|(_, bytes)| bytes)
            .sum(),
    }
}

fn format_report(report: &SizeReport) -> String {
    let mut text = String::from("Largest packages:\n");
    for package in &report.largest_packages {
        let _ = writeln!(
            text,
            "  {:>12}  {} ({} files)",
            package.bytes, package.name, package.files
        );
    }
    text.push_str("Largest files:\n");
    for file in &report.largest_files {
        let _ = writeln!(text, "  {:>12}  {}", file.bytes, file.path);
    }
    let _ = write!(
        text,
        "Source maps: {} bytes; .next/static: {} bytes",
        report.source_map_bytes, report.next_static_bytes
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upload::tests::{Fixture, fixture};
    use std::time::{Duration, SystemTime};

    const HANDLER_SOURCE: &str = "module.exports = async (req, res) => {\n  res.setHeader('content-type', 'application/json');\n  res.end(JSON.stringify({ url: req.url, host: req.headers.host }));\n};\n";

    /// A version-2 output that looks Linux-built and contains a working Lambda runtime:
    /// the generated files, a stub @next/routing and one adapter output.
    fn lambda_fixture() -> Result<Fixture> {
        let output = fixture()?;
        let runtime = output.path().join("runtime");
        for (name, source) in [
            ("runtime.cjs", include_str!("runtime.cjs")),
            ("lambda-adapter.cjs", include_str!("lambda_adapter.cjs")),
            ("lambda-entry.cjs", include_str!("lambda_entry.cjs")),
        ] {
            fs::write(runtime.join(name), source)?;
        }
        let routing = runtime.join("node_modules").join("@next").join("routing");
        fs::create_dir_all(&routing)?;
        fs::write(routing.join("package.json"), r#"{"name":"@next/routing"}"#)?;
        fs::write(
            routing.join("index.js"),
            "exports.resolveRoutes = async ({ url, pathnames }) => pathnames.includes(url.pathname) ? { resolvedPathname: url.pathname } : {};\n",
        )?;
        fs::write(runtime.join("handler.js"), HANDLER_SOURCE)?;
        fs::write(runtime.join("handler.js.map"), "{}")?;
        fs::write(
            runtime.join(".next").join("meshscale-adapter.json"),
            serde_json::json!({
                "version": 1, "buildId": "test", "config": {"basePath": ""}, "routing": {},
                "outputs": {"appRoutes": [{"id": "any", "pathname": "/any", "filePath": "handler.js"}]}
            })
            .to_string(),
        )?;
        set_platform(&output, "linux", "x86_64", "v24.15.0")?;
        Ok(output)
    }

    fn set_platform(output: &Fixture, os: &str, arch: &str, node: &str) -> Result<()> {
        let mut metadata = manifest::load(output.path())?;
        let platform = metadata
            .platform
            .as_mut()
            .context("fixture has a platform")?;
        platform.os = os.into();
        platform.arch = arch.into();
        platform.node_version = node.into();
        manifest::write(output.path(), &metadata)
    }

    fn args(output: &Path, out: &Path) -> LambdaArgs {
        LambdaArgs {
            output: output.to_path_buf(),
            arch: Architecture::X86_64,
            node_runtime: "nodejs24.x".into(),
            out: Some(out.to_path_buf()),
        }
    }

    fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
        for entry in WalkDir::new(source) {
            let entry = entry?;
            let target = destination.join(entry.path().strip_prefix(source)?);
            if entry.file_type().is_dir() {
                fs::create_dir_all(&target)?;
            } else {
                fs::copy(entry.path(), &target)?;
                // Different modification times must not change the zip.
                fs::File::options()
                    .write(true)
                    .open(&target)?
                    .set_modified(SystemTime::now() + Duration::from_secs(3600))?;
            }
        }
        Ok(())
    }

    #[test]
    fn packages_a_deterministic_zip_with_fixed_metadata() -> Result<()> {
        let output = lambda_fixture()?;
        let scratch = tempfile::TempDir::new()?;
        let first = package_lambda(&args(output.path(), &scratch.path().join("a")), LIMITS)?;
        let second = package_lambda(&args(output.path(), &scratch.path().join("b")), LIMITS)?;
        let copy = scratch.path().join("copy");
        copy_tree(output.path(), &copy)?;
        let third = package_lambda(&args(&copy, &scratch.path().join("c")), LIMITS)?;
        assert_eq!(first.lambda.zip_sha256, second.lambda.zip_sha256);
        assert_eq!(first.lambda.zip_sha256, third.lambda.zip_sha256);
        assert_eq!(fs::read(&first.zip)?, fs::read(&third.zip)?);
        assert_eq!(digest(&first.zip)?.0, first.lambda.zip_sha256);

        let mut archive = ZipArchive::new(fs::File::open(&first.zip)?)?;
        let names = (0..archive.len())
            .map(|index| Ok(archive.by_index(index)?.name().to_owned()))
            .collect::<Result<Vec<_>>>()?;
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "entries are sorted bytewise");
        for expected in [
            "lambda-entry.cjs",
            "lambda-adapter.cjs",
            "runtime.cjs",
            "empty/",
            ".next/meshscale-adapter.json",
            "node_modules/@next/routing/index.js",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "{expected} in {names:?}"
            );
        }
        assert!(!names.iter().any(|name| name == "function-entry.cjs"));
        assert!(
            !names
                .iter()
                .any(|name| name == "node_modules/" || name == ".next/"),
            "only empty directories get entries: {names:?}"
        );
        for index in 0..archive.len() {
            let file = archive.by_index(index)?;
            let expected = if file.is_dir() { 0o040_755 } else { 0o100_644 };
            assert_eq!(file.unix_mode(), Some(expected), "{}", file.name());
            assert_eq!(
                file.last_modified(),
                Some(zip::DateTime::DEFAULT),
                "{}",
                file.name()
            );
            assert!(
                file.extra_data().is_none_or(<[u8]>::is_empty),
                "{}",
                file.name()
            );
            if !file.is_dir() {
                assert_eq!(file.compression(), CompressionMethod::Deflated);
            }
        }

        let sidecar: serde_json::Value = serde_json::from_slice(&fs::read(&first.sidecar)?)?;
        assert_eq!(sidecar["schema_version"], 1);
        assert_eq!(sidecar["handler"], "lambda-entry.handler");
        assert_eq!(sidecar["runtime"], "nodejs24.x");
        assert_eq!(sidecar["architecture"], "x86_64");
        assert_eq!(sidecar["node_major"], 24);
        assert_eq!(sidecar["build_id"], "build_789");
        assert_eq!(sidecar["zip_sha256"], first.lambda.zip_sha256.as_str());
        assert_eq!(sidecar["zip_bytes"], fs::metadata(&first.zip)?.len());
        assert_eq!(sidecar["requires_s3_upload"], false);
        assert_eq!(
            sidecar["file_count"],
            names.iter().filter(|name| !name.ends_with('/')).count()
        );
        assert_eq!(sidecar["report"]["next_static_bytes"], "fallback".len());
        assert_eq!(sidecar["report"]["source_map_bytes"], 2);
        assert!(
            sidecar["report"]["largest_packages"]
                .as_array()
                .context("packages")?
                .iter()
                .any(|package| package["name"] == "@next/routing")
        );

        // The default destination is <OUTPUT>/lambda, which upload accepts and never uploads.
        let default = package_lambda(
            &LambdaArgs {
                out: None,
                ..args(output.path(), output.path())
            },
            LIMITS,
        )?;
        assert_eq!(default.zip, output.path().join("lambda").join("lambda.zip"));
        assert_eq!(default.lambda.zip_sha256, first.lambda.zip_sha256);
        Ok(())
    }

    fn package_error(output: &Fixture, configure: impl FnOnce(&mut LambdaArgs)) -> String {
        let scratch = tempfile::TempDir::new().unwrap();
        let mut arguments = args(output.path(), &scratch.path().join("out"));
        configure(&mut arguments);
        let error = package_lambda(&arguments, LIMITS).unwrap_err();
        assert!(!scratch.path().join("out").join("lambda.zip").exists());
        format!("{error:#}")
    }

    #[test]
    fn every_check_fails_with_a_specific_error() -> Result<()> {
        // 1. Version-2 output.
        let output = lambda_fixture()?;
        let mut metadata = manifest::load(output.path())?;
        metadata.version = 1;
        manifest::write(output.path(), &metadata)?;
        assert!(package_error(&output, |_| {}).contains("version-2"));

        // 2. Generated Lambda files.
        let output = lambda_fixture()?;
        fs::remove_file(output.path().join("runtime").join("lambda-entry.cjs"))?;
        assert!(package_error(&output, |_| {}).contains("runtime/lambda-entry.cjs is missing"));

        // 3. Platform: OS, architecture, Node.js major.
        let output = lambda_fixture()?;
        set_platform(&output, "windows", "x86_64", "v24.15.0")?;
        assert!(package_error(&output, |_| {}).contains("built on windows"));
        set_platform(&output, "linux", "aarch64", "v24.15.0")?;
        let error = package_error(&output, |_| {});
        assert!(
            error.contains("aarch64") && error.contains("--arch is x86_64"),
            "{error}"
        );
        set_platform(&output, "linux", "x86_64", "v24.15.0")?;
        assert!(
            package_error(&output, |arguments| arguments.arch = Architecture::Arm64)
                .contains("--arch is arm64")
        );
        set_platform(&output, "linux", "riscv64", "v24.15.0")?;
        assert!(package_error(&output, |_| {}).contains("unsupported architecture riscv64"));
        set_platform(&output, "linux", "x86_64", "v22.20.0")?;
        let error = package_error(&output, |_| {});
        assert!(
            error.contains("v22.20.0") && error.contains("nodejs24.x"),
            "{error}"
        );
        let scratch = tempfile::TempDir::new()?;
        package_lambda(
            &LambdaArgs {
                node_runtime: "nodejs22.x".into(),
                ..args(output.path(), scratch.path())
            },
            LIMITS,
        )?;

        // 4. Package contents.
        let output = lambda_fixture()?;
        fs::write(
            output.path().join("runtime").join(".env.production"),
            "SECRET=1",
        )?;
        assert!(package_error(&output, |_| {}).contains("dotenv"));
        let output = lambda_fixture()?;
        let target = tempfile::TempDir::new()?;
        crate::artifact::tests::link_directory(
            target.path(),
            &output.path().join("runtime").join("linked"),
        )?;
        assert!(package_error(&output, |_| {}).contains("link"));

        // 5. Next.js with the stable Adapter API.
        let output = lambda_fixture()?;
        let mut metadata = manifest::load(output.path())?;
        metadata.deployment.next_version = Some("16.1.6".into());
        manifest::write(output.path(), &metadata)?;
        let error = package_error(&output, |_| {});
        assert!(
            error.contains("16.1.6") && error.contains("16.2.0"),
            "{error}"
        );

        // The destination cannot be inside runtime/.
        let output = lambda_fixture()?;
        let inside = output.path().join("runtime").join("lambda");
        assert!(
            package_error(&output, |arguments| arguments.out = Some(inside))
                .contains("inside runtime/")
        );
        Ok(())
    }

    #[test]
    fn size_limits_fail_hard_or_flag_s3_upload() -> Result<()> {
        let output = lambda_fixture()?;
        let scratch = tempfile::TempDir::new()?;
        let out = scratch.path().join("out");
        let error = package_lambda(
            &args(output.path(), &out),
            Limits {
                max_unzipped_bytes: 100,
                ..LIMITS
            },
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("above the 100 byte Lambda limit"),
            "{message}"
        );
        assert!(message.contains("Largest packages:") && message.contains("@next/routing"));
        assert!(!out.join("lambda.zip").exists() && !out.join("lambda.json").exists());

        let flagged = package_lambda(
            &args(output.path(), &out),
            Limits {
                max_direct_upload_bytes: 100,
                ..LIMITS
            },
        )?;
        assert!(flagged.lambda.requires_s3_upload);
        Ok(())
    }

    #[test]
    fn size_report_groups_packages_and_answers_the_pruning_questions() {
        let file = |bytes| Entry::File {
            source: PathBuf::new(),
            bytes,
        };
        let entries = BTreeMap::from([
            ("node_modules/@scope/a/index.js".to_owned(), file(10)),
            ("node_modules/@scope/a/index.js.map".to_owned(), file(30)),
            ("node_modules/b/lib/x.js".to_owned(), file(5)),
            ("node_modules/b/node_modules/c/i.js".to_owned(), file(7)),
            (
                "node_modules/.pnpm/d@1/node_modules/d/i.js".to_owned(),
                file(1),
            ),
            (".next/static/chunks/a.js".to_owned(), file(4)),
            ("empty/".to_owned(), Entry::Directory),
        ]);
        let report = size_report(&entries);
        let packages = report
            .largest_packages
            .iter()
            .map(|package| (package.name.as_str(), package.bytes, package.files))
            .collect::<Vec<_>>();
        assert_eq!(
            packages,
            vec![("@scope/a", 40, 2), ("b", 12, 2), (".pnpm", 1, 1)]
        );
        assert_eq!(
            report.largest_files[0].path,
            "node_modules/@scope/a/index.js.map"
        );
        assert_eq!(report.source_map_bytes, 30);
        assert_eq!(report.next_static_bytes, 4);
    }

    #[test]
    fn unzipped_package_answers_events_in_an_empty_directory() -> Result<()> {
        let output = lambda_fixture()?;
        let scratch = tempfile::TempDir::new()?;
        let packaged = package_lambda(&args(output.path(), &scratch.path().join("out")), LIMITS)?;
        // Like Lambda, the package is extracted into an otherwise empty directory and the
        // handler runs with that directory as its working directory.
        let task_root = scratch.path().join("task");
        fs::create_dir(&task_root)?;
        ZipArchive::new(fs::File::open(&packaged.zip)?)?.extract(&task_root)?;
        let script = r#"
const { handler } = require('./lambda-entry.cjs');
const event = (rawPath, rawQueryString, headers = {}) => ({
  version: '2.0', rawPath, rawQueryString,
  headers: { host: 'abc.lambda-url.us-east-1.on.aws', 'x-forwarded-host': 'app.test', ...headers },
  requestContext: { http: { method: 'GET', path: rawPath } }, isBase64Encoded: false,
});
(async () => {
  const results = [
    await handler(event('/', '', { 'x-meshscale-probe': 'init' }), {}),
    await handler(event('/any', 'a=1&a=2'), {}),
    await handler(event('/missing', ''), {}),
  ];
  process.stderr.write('RESULTS' + JSON.stringify(results));
})().catch((error) => { console.error(error); process.exitCode = 1; });
"#;
        let result = std::process::Command::new("node")
            .args(["-e", script])
            .current_dir(&task_root)
            .output()?;
        let stderr = String::from_utf8_lossy(&result.stderr);
        ensure!(result.status.success(), "handler failed: {stderr}");
        let results: serde_json::Value = serde_json::from_str(
            stderr
                .split_once("RESULTS")
                .context("no results printed")?
                .1,
        )?;
        assert_eq!(results[0]["statusCode"], 200);
        assert_eq!(results[1]["statusCode"], 200);
        assert_eq!(
            results[1]["body"],
            r#"{"url":"/any?a=1&a=2","host":"app.test"}"#
        );
        assert_eq!(results[2]["statusCode"], 404);
        Ok(())
    }
}
