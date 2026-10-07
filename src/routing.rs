use crate::manifest::StaticObject;
use anyhow::{Context, Result, ensure};
use axum::http::{HeaderMap, Method, Uri};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    pub semantics: String,
    pub rules: Vec<Rule>,
    pub fallback_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Rule {
    ServerVariants {
        methods_except: Vec<String>,
        headers_present: Vec<String>,
        cookies_present: Vec<String>,
        query_present: Vec<String>,
    },
    Static {
        path: String,
        object: String,
        methods: Vec<String>,
        query: QueryPolicy,
    },
    Server {},
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum QueryPolicy {
    Ignore,
    Empty,
}

impl Routing {
    pub fn new() -> Self {
        Self {
            semantics: "meshscale-routing-v1".to_owned(),
            rules: vec![Self::variants(), Rule::Server {}],
            fallback_reasons: Vec::new(),
        }
    }

    fn variants() -> Rule {
        Rule::ServerVariants {
            methods_except: vec!["GET".into(), "HEAD".into()],
            headers_present: [
                "rsc",
                "next-action",
                "next-router-state-tree",
                "next-router-prefetch",
                "next-router-segment-prefetch",
                "x-prerender-revalidate",
                "x-prerender-revalidate-if-generated",
                "x-next-revalidated-tags",
                "x-next-revalidate-tag-token",
                "x-middleware-prefetch",
            ]
            .map(str::to_owned)
            .to_vec(),
            cookies_present: ["__prerender_bypass", "__next_preview_data"]
                .map(str::to_owned)
                .to_vec(),
            query_present: vec!["_rsc".to_owned()],
        }
    }

    pub fn insert_static(
        &mut self,
        path: String,
        object: String,
        query: QueryPolicy,
    ) -> Result<()> {
        ensure!(
            !self
                .rules
                .iter()
                .any(|rule| matches!(rule, Rule::Static {path: old, ..} if old == &path)),
            "multiple static responses claim URL {path}"
        );
        self.rules.insert(
            self.rules.len() - 1,
            Rule::Static {
                path,
                object,
                methods: vec!["GET".into(), "HEAD".into()],
                query,
            },
        );
        Ok(())
    }

    pub fn validate(&self, objects: &BTreeMap<String, StaticObject>) -> Result<()> {
        ensure!(
            self.semantics == "meshscale-routing-v1",
            "unsupported routing semantics"
        );
        ensure!(
            self.rules.len() >= 2,
            "routing must have variant bypass and server fallback"
        );
        // Enforce the safety bypass contract, not merely its descriptive reason.
        ensure!(
            serde_json::to_value(&self.rules[0])? == serde_json::to_value(Self::variants())?,
            "missing required Next.js request-variant bypass"
        );
        ensure!(
            matches!(self.rules.last(), Some(Rule::Server {})),
            "missing catch-all server rule"
        );
        let mut paths = BTreeSet::new();
        for rule in &self.rules[1..self.rules.len() - 1] {
            let Rule::Static {
                path,
                object,
                methods,
                ..
            } = rule
            else {
                anyhow::bail!("only exact static rules are supported between bypass and fallback");
            };
            ensure!(
                path.starts_with('/')
                    && !path.contains([':', '\\', '\0'])
                    && path.split('/').all(|part| part != "." && part != ".."),
                "invalid static URL path"
            );
            ensure!(paths.insert(path), "duplicate static URL");
            ensure!(methods == &["GET", "HEAD"], "invalid static methods");
            objects
                .get(object)
                .context("static route references unknown object")?;
        }
        Ok(())
    }

    pub fn select<'a>(
        &'a self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
    ) -> Result<Option<&'a str>> {
        let path = decoded_path(uri.path())?;
        for rule in &self.rules {
            match rule {
                Rule::ServerVariants {
                    methods_except,
                    headers_present,
                    cookies_present,
                    query_present,
                } => {
                    let query_names = uri
                        .query()
                        .unwrap_or("")
                        .split('&')
                        .map(|part| {
                            percent_encoding::percent_decode_str(
                                part.split('=').next().unwrap_or(""),
                            )
                            .decode_utf8_lossy()
                            .into_owned()
                        })
                        .collect::<Vec<_>>();
                    let cookies = headers
                        .get_all("cookie")
                        .iter()
                        .filter_map(|value| value.to_str().ok())
                        .flat_map(|value| value.split(';'))
                        .map(|part| part.trim().split('=').next().unwrap_or(""))
                        .collect::<Vec<_>>();
                    if !methods_except.iter().any(|value| value == method.as_str())
                        || headers_present
                            .iter()
                            .any(|name| headers.contains_key(name))
                        || cookies_present
                            .iter()
                            .any(|name| cookies.contains(&name.as_str()))
                        || query_present.iter().any(|name| query_names.contains(name))
                    {
                        return Ok(None);
                    }
                }
                Rule::Static {
                    path: expected,
                    object,
                    methods,
                    query,
                } if expected == &path
                    && methods.iter().any(|value| value == method.as_str())
                    && (*query == QueryPolicy::Ignore || uri.query().is_none_or(str::is_empty)) =>
                {
                    return Ok(Some(object));
                }
                Rule::Server {} => return Ok(None),
                _ => {}
            }
        }
        Ok(None)
    }
}

pub fn decoded_path(path: &str) -> Result<String> {
    let bytes = path.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%' {
            ensure!(
                bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
                    && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit),
                "invalid URL escape"
            );
        }
    }
    let path = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .context("invalid URL UTF-8")?;
    ensure!(
        path.starts_with('/') && !path.contains(['\\', ':', '\0']),
        "invalid request path"
    );
    ensure!(
        path.split('/').all(|part| part != "." && part != ".."),
        "request path traversal"
    );
    Ok(path.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bypasses_variants_and_matches_only_exact_safe_paths() -> Result<()> {
        let mut routing = Routing::new();
        routing.insert_static(
            "/about".into(),
            "_prerender/about.html".into(),
            QueryPolicy::Empty,
        )?;
        for (method, path, header, expected) in [
            ("GET", "/about", None, true),
            ("HEAD", "/about", None, true),
            ("POST", "/about", None, false),
            ("GET", "/about/", None, false),
            ("GET", "/about?q=1", None, false),
            ("GET", "/about?_rsc=x", None, false),
            ("GET", "/about", Some(("rsc", "1")), false),
            (
                "GET",
                "/about",
                Some(("cookie", "__prerender_bypass=token")),
                false,
            ),
        ] {
            let mut headers = HeaderMap::new();
            if let Some((key, value)) = header {
                headers.insert(axum::http::HeaderName::from_static(key), value.parse()?);
            }
            assert_eq!(
                routing
                    .select(&method.parse()?, &path.parse()?, &headers)?
                    .is_some(),
                expected
            );
        }
        assert!(decoded_path("/%2e%2e/file").is_err());
        assert!(decoded_path("/%GG").is_err());
        Ok(())
    }
}
