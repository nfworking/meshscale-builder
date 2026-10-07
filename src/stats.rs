use crate::cache::CacheSnapshot;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
use tracing::info;

/// Where a request was ultimately answered from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Static bytes already resident in the in-memory cache.
    CacheHit,
    /// Static bytes read from disk and inserted into the cache by this request.
    CacheFill,
    /// Static file streamed from disk (cache disabled, file too large, ranges, or v1 artifact).
    StaticStream,
    /// Static 304/412 answered from manifest validators without reading the file body.
    StaticConditional,
    /// Forwarded to the Node runtime because no static rule matched.
    Server,
    /// A static rule matched but its file was missing, so the request went to Node.
    ServerStaticMissing,
    /// Rejected or failed locally (bad path, upgrade, internal error).
    Rejected,
}

impl Source {
    const ALL: [Source; 7] = [
        Source::CacheHit,
        Source::CacheFill,
        Source::StaticStream,
        Source::StaticConditional,
        Source::Server,
        Source::ServerStaticMissing,
        Source::Rejected,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::CacheHit => "cache-hit",
            Self::CacheFill => "cache-fill",
            Self::StaticStream => "static-disk",
            Self::StaticConditional => "static-304",
            Self::Server => "server",
            Self::ServerStaticMissing => "server-static-missing",
            Self::Rejected => "rejected",
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|source| *source == self)
            .expect("every source is listed")
    }
}

pub struct Stats {
    started: Instant,
    counts: [AtomicU64; 7],
    cache_hit_bytes: AtomicU64,
    last_logged_total: AtomicU64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            counts: Default::default(),
            cache_hit_bytes: AtomicU64::new(0),
            last_logged_total: AtomicU64::new(0),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Totals {
    pub requests: u64,
    pub static_requests: u64,
    pub server_requests: u64,
    /// Share of static responses with a body that were served from memory.
    pub cache_hit_rate: Option<f64>,
}

impl Stats {
    pub fn record(&self, source: Source, bytes_from_memory: u64) {
        self.counts[source.index()].fetch_add(1, Ordering::Relaxed);
        if source == Source::CacheHit {
            self.cache_hit_bytes
                .fetch_add(bytes_from_memory, Ordering::Relaxed);
        }
    }

    pub fn count(&self, source: Source) -> u64 {
        self.counts[source.index()].load(Ordering::Relaxed)
    }

    pub fn totals(&self) -> Totals {
        let hits = self.count(Source::CacheHit);
        let bodies = hits + self.count(Source::CacheFill) + self.count(Source::StaticStream);
        let static_requests = bodies + self.count(Source::StaticConditional);
        let server_requests = self.count(Source::Server) + self.count(Source::ServerStaticMissing);
        Totals {
            requests: static_requests + server_requests + self.count(Source::Rejected),
            static_requests,
            server_requests,
            cache_hit_rate: (bodies > 0).then(|| hits as f64 / bodies as f64 * 100.0),
        }
    }

    /// Logs a periodic summary only when traffic arrived since the previous summary.
    pub fn log_if_changed(&self, cache: &CacheSnapshot) {
        let total = self.totals().requests;
        if self.last_logged_total.swap(total, Ordering::Relaxed) != total {
            self.log("stats", cache);
        }
    }

    pub fn log(&self, label: &str, cache: &CacheSnapshot) {
        let totals = self.totals();
        let hit_rate = if cache.capacity == 0 {
            "disabled".to_owned()
        } else {
            totals
                .cache_hit_rate
                .map_or_else(|| "n/a".to_owned(), |rate| format!("{rate:.1}%"))
        };
        info!(
            uptime_s = self.started.elapsed().as_secs(),
            requests = totals.requests,
            static_total = totals.static_requests,
            server_total = totals.server_requests,
            cache_hits = self.count(Source::CacheHit),
            cache_fills = self.count(Source::CacheFill),
            static_disk = self.count(Source::StaticStream),
            static_304 = self.count(Source::StaticConditional),
            server = self.count(Source::Server),
            server_static_missing = self.count(Source::ServerStaticMissing),
            rejected = self.count(Source::Rejected),
            cache_hit_rate = %hit_rate,
            memory_bytes_served = self.cache_hit_bytes.load(Ordering::Relaxed),
            cache_entries = cache.entries,
            cache_bytes = cache.bytes,
            cache_capacity_bytes = cache.capacity,
            cache_disk_reads = cache.fills,
            cache_evictions = cache.evictions,
            "{label}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_separate_static_server_and_cache_hit_rate() {
        let stats = Stats::default();
        assert_eq!(stats.totals().cache_hit_rate, None);
        stats.record(Source::CacheFill, 0);
        stats.record(Source::CacheHit, 10);
        stats.record(Source::CacheHit, 5);
        stats.record(Source::StaticStream, 0);
        stats.record(Source::StaticConditional, 0);
        stats.record(Source::Server, 0);
        stats.record(Source::ServerStaticMissing, 0);
        stats.record(Source::Rejected, 0);
        let totals = stats.totals();
        assert_eq!(totals.requests, 8);
        assert_eq!(totals.static_requests, 5);
        assert_eq!(totals.server_requests, 2);
        assert_eq!(totals.cache_hit_rate, Some(50.0));
        assert_eq!(stats.cache_hit_bytes.load(Ordering::Relaxed), 15);
    }
}
