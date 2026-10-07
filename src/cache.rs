use crate::manifest::StaticObject;
use anyhow::{Result, ensure};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{io::AsyncReadExt, sync::Mutex};

struct Entry {
    bytes: Bytes,
    touched: u64,
}
#[derive(Default)]
struct CacheState {
    entries: BTreeMap<String, Entry>,
    bytes: u64,
    clock: u64,
}

pub struct StaticCache {
    capacity: u64,
    max_file: u64,
    state: Mutex<CacheState>,
    // One fill at a time bounds read buffers and coalesces concurrent misses.
    fill: Mutex<()>,
    reads: AtomicU64,
}

impl StaticCache {
    pub fn new(capacity: u64, max_file: u64) -> Result<Arc<Self>> {
        ensure!(
            capacity == 0 || (max_file > 0 && max_file <= capacity),
            "static cache per-file limit must be positive and no greater than total budget"
        );
        Ok(Arc::new(Self {
            capacity,
            max_file,
            state: Mutex::new(CacheState::default()),
            fill: Mutex::new(()),
            reads: AtomicU64::new(0),
        }))
    }

    async fn hit(&self, key: &str) -> Option<Bytes> {
        let mut state = self.state.lock().await;
        state.clock += 1;
        let clock = state.clock;
        state.entries.get_mut(key).map(|entry| {
            entry.touched = clock;
            entry.bytes.clone()
        })
    }

    pub async fn cached(&self, key: &str, object: &StaticObject) -> Option<Bytes> {
        self.hit(&format!("{key}:{}", object.sha256)).await
    }

    pub async fn get(
        &self,
        key: &str,
        path: &Path,
        object: &StaticObject,
    ) -> Result<Option<Bytes>> {
        if self.capacity == 0 || object.bytes > self.max_file || object.bytes > self.capacity {
            return Ok(None);
        }
        let key = format!("{key}:{}", object.sha256);
        if let Some(bytes) = self.hit(&key).await {
            return Ok(Some(bytes));
        }
        let _fill = self.fill.lock().await;
        if let Some(bytes) = self.hit(&key).await {
            return Ok(Some(bytes));
        }
        {
            let mut state = self.state.lock().await;
            while state.bytes + object.bytes > self.capacity {
                let oldest = state
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.touched)
                    .map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    state.bytes -= state
                        .entries
                        .remove(&oldest)
                        .expect("entry selected from map")
                        .bytes
                        .len() as u64;
                } else {
                    break;
                }
            }
        }
        let size = usize::try_from(object.bytes)?;
        let mut bytes = vec![0; size];
        let mut file = tokio::fs::File::open(path).await?;
        file.read_exact(&mut bytes).await?;
        let mut extra = [0; 1];
        ensure!(
            file.read(&mut extra).await? == 0,
            "static file exceeds manifest size"
        );
        self.reads.fetch_add(1, Ordering::Relaxed);
        ensure!(
            bytes.len() == size && format!("{:x}", Sha256::digest(&bytes)) == object.sha256,
            "static file changed since build; restart/rebuild the artifact"
        );
        let bytes = Bytes::from(bytes);
        let mut state = self.state.lock().await;
        state.clock += 1;
        let clock = state.clock;
        state.bytes += bytes.len() as u64;
        state.entries.insert(
            key,
            Entry {
                bytes: bytes.clone(),
                touched: clock,
            },
        );
        Ok(Some(bytes))
    }
}

pub fn apply_metadata(response: &mut Response, object: &StaticObject) -> Result<()> {
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, object.content_type.parse()?);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, object.cache_control.parse()?);
    response
        .headers_mut()
        .insert(header::ETAG, object.etag().parse()?);
    response
        .headers_mut()
        .insert(header::LAST_MODIFIED, object.last_modified.parse()?);
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, "bytes".parse()?);
    for (name, value) in &object.headers {
        response.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes())?,
            value.parse()?,
        );
    }
    if response.status() == StatusCode::OK {
        *response.status_mut() = StatusCode::from_u16(object.status)?;
    }
    Ok(())
}

pub fn conditional_status(headers: &HeaderMap, object: &StaticObject) -> Option<StatusCode> {
    let etag = object.etag();
    if headers
        .get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            !v.split(',')
                .any(|tag| tag.trim() == "*" || tag.trim() == etag)
        })
    {
        return Some(StatusCode::PRECONDITION_FAILED);
    }
    let modified = httpdate::parse_http_date(&object.last_modified).ok()?;
    if !headers.contains_key(header::IF_MATCH)
        && headers
            .get(header::IF_UNMODIFIED_SINCE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| httpdate::parse_http_date(v).ok())
            .is_some_and(|date| modified > date)
    {
        return Some(StatusCode::PRECONDITION_FAILED);
    }
    if let Some(value) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if value
            .split(',')
            .any(|tag| tag.trim() == "*" || tag.trim().trim_start_matches("W/") == etag)
        {
            return Some(StatusCode::NOT_MODIFIED);
        }
    } else if headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| httpdate::parse_http_date(v).ok())
        .is_some_and(|date| modified <= date)
    {
        return Some(StatusCode::NOT_MODIFIED);
    }
    None
}

/// Multi-range requests use the filesystem service instead of buffering a multipart response.
pub fn response(
    bytes: Bytes,
    method: &Method,
    headers: &HeaderMap,
    object: &StaticObject,
) -> Result<Option<Response>> {
    if let Some(status) = conditional_status(headers, object) {
        let mut response = status.into_response();
        apply_metadata(&mut response, object)?;
        return Ok(Some(response));
    }
    let mut status = StatusCode::OK;
    let mut body = bytes.clone();
    let mut content_range = None;
    let allow_range = headers
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| {
            v == object.etag()
                || httpdate::parse_http_date(v)
                    .ok()
                    .zip(httpdate::parse_http_date(&object.last_modified).ok())
                    .is_some_and(|(date, modified)| date >= modified)
        });
    if method == Method::GET
        && allow_range
        && let Some(range) = headers.get(header::RANGE).and_then(|v| v.to_str().ok())
    {
        if range.contains(',') {
            return Ok(None);
        }
        if let Some(range) = range.strip_prefix("bytes=") {
            let parsed = range.split_once('-').and_then(|(start, end)| {
                let length = bytes.len() as u64;
                if start.is_empty() {
                    let suffix: u64 = end.parse().ok()?;
                    (suffix > 0 && length > 0)
                        .then(|| (length.saturating_sub(suffix), length - 1))
                } else {
                    let start: u64 = start.parse().ok()?;
                    let end = if end.is_empty() {
                        length.saturating_sub(1)
                    } else {
                        end.parse::<u64>().ok()?.min(length.saturating_sub(1))
                    };
                    (start <= end && start < length).then_some((start, end))
                }
            });
            if let Some((start, end)) = parsed {
                body = bytes.slice(start as usize..=end as usize);
                status = StatusCode::PARTIAL_CONTENT;
                content_range = Some(format!("bytes {start}-{end}/{}", bytes.len()));
            } else {
                let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                response.headers_mut().insert(
                    header::CONTENT_RANGE,
                    format!("bytes */{}", bytes.len()).parse()?,
                );
                apply_metadata(&mut response, object)?;
                return Ok(Some(response));
            }
        }
    }
    let length = body.len();
    let mut response = (
        status,
        if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(body)
        },
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, length.to_string().parse()?);
    if let Some(range) = content_range {
        response
            .headers_mut()
            .insert(header::CONTENT_RANGE, range.parse()?);
    }
    apply_metadata(&mut response, object)?;
    Ok(Some(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn coalesces_misses_evicts_lru_and_rejects_mutation() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, b"1234")?;
        std::fs::write(&b, b"5678")?;
        let oa = crate::static_output::object(&a, "public_asset")?;
        let ob = crate::static_output::object(&b, "public_asset")?;
        let cache = StaticCache::new(4, 4)?;
        let (first, second) = tokio::join!(cache.get("a", &a, &oa), cache.get("a", &a, &oa));
        assert_eq!(first?.unwrap(), second?.unwrap());
        assert_eq!(cache.reads.load(Ordering::Relaxed), 1);
        cache.get("b", &b, &ob).await?;
        assert_eq!(cache.state.lock().await.bytes, 4);
        std::fs::write(&a, b"bad!")?;
        assert!(cache.get("a", &a, &oa).await.is_err());
        assert_eq!(cache.reads.load(Ordering::Relaxed), 3);
        assert!(StaticCache::new(0, 4)?.get("b", &b, &ob).await?.is_none());
        assert!(StaticCache::new(4, 3)?.get("b", &b, &ob).await?.is_none());
        assert!(StaticCache::new(4, 5).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn exact_default_limits_and_disabled_cache_are_enforced() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("large");
        let bytes = vec![b'x'; 16 * 1024 * 1024];
        std::fs::write(&path, &bytes)?;
        let mut object = crate::static_output::object(&path, "public_asset")?;
        let cache = StaticCache::new(256 * 1024 * 1024, 16 * 1024 * 1024)?;
        assert_eq!(
            cache.get("large", &path, &object).await?.unwrap().len(),
            bytes.len()
        );
        object.bytes += 1;
        assert!(cache.get("oversized", &path, &object).await?.is_none());
        assert_eq!(cache.reads.load(Ordering::Relaxed), 1);
        assert_eq!(cache.state.lock().await.bytes, 16 * 1024 * 1024);
        Ok(())
    }

    #[tokio::test]
    async fn cached_responses_preserve_head_range_and_conditional_semantics() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "0123456789")?;
        let object = crate::static_output::object(&path, "public_asset")?;
        let bytes = Bytes::from_static(b"0123456789");
        for (method, range, status, body) in [
            (Method::GET, None, StatusCode::OK, "0123456789"),
            (Method::HEAD, None, StatusCode::OK, ""),
            (
                Method::GET,
                Some("bytes=2-4"),
                StatusCode::PARTIAL_CONTENT,
                "234",
            ),
            (
                Method::GET,
                Some("bytes=-3"),
                StatusCode::PARTIAL_CONTENT,
                "789",
            ),
            (
                Method::GET,
                Some("bytes=30-40"),
                StatusCode::RANGE_NOT_SATISFIABLE,
                "",
            ),
        ] {
            let mut headers = HeaderMap::new();
            if let Some(range) = range {
                headers.insert(header::RANGE, range.parse()?);
            }
            let result = response(bytes.clone(), &method, &headers, &object)?.unwrap();
            assert_eq!(result.status(), status);
            assert_eq!(result.headers()[header::ETAG], object.etag());
            assert_eq!(axum::body::to_bytes(result.into_body(), 100).await?, body);
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            format!("W/{}", object.etag()).parse()?,
        );
        assert_eq!(
            response(bytes.clone(), &Method::GET, &headers, &object)?
                .unwrap()
                .status(),
            StatusCode::NOT_MODIFIED
        );
        headers.clear();
        headers.insert(header::IF_MATCH, "\"other\"".parse()?);
        assert_eq!(
            response(bytes.clone(), &Method::GET, &headers, &object)?
                .unwrap()
                .status(),
            StatusCode::PRECONDITION_FAILED
        );
        headers.clear();
        headers.insert(header::RANGE, "bytes=0-1,4-5".parse()?);
        assert!(response(bytes, &Method::GET, &headers, &object)?.is_none());
        headers.insert(header::RANGE, "bytes=-1".parse()?);
        assert_eq!(response(Bytes::new(), &Method::GET, &headers, &object)?.unwrap().status(),
            StatusCode::RANGE_NOT_SATISFIABLE);
        Ok(())
    }
}
