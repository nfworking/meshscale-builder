use anyhow::{Context, Result, bail};
use std::{ffi::OsString, fs, path::Path, process::Command};

/// Variables inherited from the builder's environment by build-time children. Everything
/// else is cleared: install scripts and `next build` run arbitrary repository code.
const INHERITED: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "TZ",
    "TMPDIR",
    "TEMP",
    "TMP",
    "CI",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "NODE_EXTRA_CA_CERTS",
];
const INHERITED_LOWERCASE: &[&str] = &["http_proxy", "https_proxy", "no_proxy"];
const INHERITED_WINDOWS: &[&str] = &[
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "HOMEDRIVE",
    "HOMEPATH",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
];

/// Clears `command`'s environment, then sets only the allowlisted variables from the
/// builder's environment and the explicit project variables (which take precedence).
/// Callers set their own fixed variables (`CI`, `NEXT_ADAPTER_PATH`, ...) afterwards.
pub fn restrict_build_env(command: &mut Command, project_vars: &[(String, String)]) {
    restrict_env_from(command, std::env::vars_os(), project_vars);
}

fn restrict_env_from(
    command: &mut Command,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    project_vars: &[(String, String)],
) {
    command.env_clear();
    for (name, value) in inherited {
        if name.to_str().is_some_and(is_inherited) {
            command.env(name, value);
        }
    }
    for (name, value) in project_vars {
        command.env(name, value);
    }
}

fn is_inherited(name: &str) -> bool {
    if cfg!(windows) {
        // Windows variable names are case-insensitive ("Path", "SystemRoot", "ComSpec").
        let upper = name.to_ascii_uppercase();
        INHERITED.contains(&upper.as_str())
            || INHERITED_WINDOWS.contains(&upper.as_str())
            || upper.starts_with("LC_")
    } else {
        INHERITED.contains(&name) || INHERITED_LOWERCASE.contains(&name) || name.starts_with("LC_")
    }
}

/// Loads `--build-env-file`. Parse errors never echo file content.
pub fn load_build_env_file(path: &Path) -> Result<Vec<(String, String)>> {
    // dotenvy expands `$NAME`/`${NAME}` from the builder's own process environment, which
    // would let the file copy builder credentials into the build. Refuse `$` entirely.
    let content = fs::read(path)
        .map_err(|_| anyhow::anyhow!("failed to read build env file {}", path.display()))?;
    for (index, line) in content.split(|byte| *byte == b'\n').enumerate() {
        let trimmed = line.trim_ascii_start();
        if !trimmed.starts_with(b"#") && line.contains(&b'$') {
            bail!(
                "build env file {} line {}: '$' is not supported (variable expansion would read the builder's environment)",
                path.display(),
                index + 1
            );
        }
    }
    let values = crate::upload::load_dotenv(Some(path))
        .with_context(|| format!("failed to parse build env file {}", path.display()))?;
    for name in values.keys() {
        if is_reserved(name) {
            bail!(
                "build env file {} must not set {name}: builder credentials and AWS_* variables are never passed to builds",
                path.display()
            );
        }
    }
    Ok(values.into_iter().collect())
}

fn is_reserved(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("AWS_")
        || upper == "MESHSCALE_GITHUB_TOKEN"
        || crate::upload::ENV_NAMES.contains(&upper.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn child_env(command: &mut Command) -> Result<BTreeMap<String, String>> {
        let output = command
            .args(["-e", "process.stdout.write(JSON.stringify(process.env))"])
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    fn path_from(environment: &BTreeMap<String, String>) -> Option<&String> {
        environment
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("PATH"))
            .map(|(_, value)| value)
    }

    #[test]
    fn child_sees_only_allowlisted_and_project_variables() -> Result<()> {
        let mut inherited: Vec<(OsString, OsString)> = std::env::vars_os().collect();
        for name in [
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "MESHSCALE_R2_SECRET_ACCESS_KEY",
            "MESHScale_R2_SECRET_ACCESS_KEY",
            "MESHSCALE_GITHUB_TOKEN",
            "UNRELATED_CLOUD_TOKEN",
        ] {
            inherited.push((name.into(), "planted-secret".into()));
        }
        let mut command = Command::new("node");
        restrict_env_from(
            &mut command,
            inherited,
            &[("NEXT_PUBLIC_TEST_VALUE".into(), "project-value".into())],
        );
        let environment = child_env(&mut command)?;
        assert!(
            !environment.values().any(|value| value == "planted-secret"),
            "{:?}",
            environment.keys()
        );
        assert_eq!(environment["NEXT_PUBLIC_TEST_VALUE"], "project-value");
        assert!(path_from(&environment).is_some_and(|path| !path.is_empty()));
        Ok(())
    }

    #[test]
    fn real_environment_is_reduced_to_the_allowlist() -> Result<()> {
        let mut command = Command::new("node");
        restrict_build_env(&mut command, &[("PROJECT_VAR".into(), "1".into())]);
        let environment = child_env(&mut command)?;
        for name in environment.keys() {
            // Windows creates a few per-process variables (for example "=C:") itself.
            if cfg!(windows) && name.starts_with('=') {
                continue;
            }
            assert!(
                is_inherited(name) || name == "PROJECT_VAR",
                "unexpected variable reached the child: {name}"
            );
        }
        assert!(path_from(&environment).is_some());
        Ok(())
    }

    #[test]
    fn project_variables_override_inherited_ones() -> Result<()> {
        let mut command = Command::new("node");
        restrict_env_from(
            &mut command,
            std::env::vars_os().chain([("TZ".into(), "inherited".into())]),
            &[("TZ".into(), "project".into())],
        );
        assert_eq!(child_env(&mut command)?["TZ"], "project");
        Ok(())
    }

    #[test]
    fn allowlist_matches_platform_case_rules() {
        for name in ["PATH", "LC_ALL", "HTTPS_PROXY", "NODE_EXTRA_CA_CERTS", "CI"] {
            assert!(is_inherited(name), "{name}");
        }
        for name in [
            "AWS_ACCESS_KEY_ID",
            "MESHSCALE_GITHUB_TOKEN",
            "MESHSCALE_R2_BUCKET",
            "NPM_TOKEN",
            "NODE_OPTIONS",
            "GITHUB_TOKEN",
        ] {
            assert!(!is_inherited(name), "{name}");
        }
        assert!(is_inherited("https_proxy"));
        assert_eq!(is_inherited("Path"), cfg!(windows));
        assert_eq!(is_inherited("SystemRoot"), cfg!(windows));
        assert_eq!(is_inherited("ProgramFiles(x86)"), cfg!(windows));
    }

    #[test]
    fn build_env_file_loads_values_and_rejects_unsafe_content() -> Result<()> {
        let directory = tempfile::TempDir::new()?;
        let path = directory.path().join("build.env");
        fs::write(
            &path,
            "# comment with $dollar\nNEXT_PUBLIC_A=one\nB='two words'\nnpm_config_registry=\"https://registry.example.test/\"\n",
        )?;
        let values = load_build_env_file(&path)?;
        assert_eq!(
            values,
            vec![
                ("B".to_owned(), "two words".to_owned()),
                ("NEXT_PUBLIC_A".to_owned(), "one".to_owned()),
                (
                    "npm_config_registry".to_owned(),
                    "https://registry.example.test/".to_owned()
                ),
            ]
        );

        for content in [
            "LEAK=${MESHSCALE_R2_SECRET_ACCESS_KEY}\n",
            "LEAK=$PATH\n",
            "A=1\nLEAK=\"prefix-$HOME\"\n",
        ] {
            fs::write(&path, content)?;
            let error = format!("{:#}", load_build_env_file(&path).unwrap_err());
            assert!(error.contains("'$' is not supported"), "{error}");
            assert!(!error.contains("MESHSCALE_R2") && !error.contains("prefix"));
        }

        for name in [
            "AWS_SECRET_ACCESS_KEY",
            "aws_region",
            "MESHSCALE_GITHUB_TOKEN",
            "MESHSCALE_R2_SECRET_ACCESS_KEY",
            "MESHScale_R2_BUCKET",
        ] {
            fs::write(&path, format!("{name}=do-not-echo\n"))?;
            let error = format!("{:#}", load_build_env_file(&path).unwrap_err());
            assert!(error.contains("must not set"), "{error}");
            assert!(!error.contains("do-not-echo"), "{error}");
        }

        fs::write(&path, "SECRET='do-not-echo")?;
        let error = format!("{:#}", load_build_env_file(&path).unwrap_err());
        assert!(!error.contains("do-not-echo"), "{error}");
        assert!(load_build_env_file(&directory.path().join("missing")).is_err());
        Ok(())
    }
}
