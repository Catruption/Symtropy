//! Headless remote-source adapters for Symtropy.
//!
//! This crate never starts a background poll and never writes into the shared
//! library index. It models user-initiated lookups, injects the network behind
//! `Fetcher`, materializes approved downloads into a local source cache, and
//! turns visually confirmed matches into sandboxed tag proposals.

pub mod adapters;
pub mod http;
pub mod ingest;
pub mod lookup;
pub mod matching;
pub mod model;
pub mod policy;

pub use adapters::{adapter_for, all_adapters, E621Adapter};
pub use http::{validate_source_url, HttpFetcher, DEFAULT_USER_AGENT};
pub use ingest::{IngestResult, Ingestor};
pub use lookup::{
    download_and_ingest, run_lookup, source_search_tags, LookupReport, LookupRequest,
    ScoredCandidate,
};
pub use matching::{dimensions, evaluate_candidate, proposal_for_match, similarity};
pub use model::{
    AdapterCapabilities, FetchRequest, FetchResponse, Fetcher, RemoteItem, RemoteTag, Result,
    SearchQuery, SourceAdapter, SourceSite,
};
pub use policy::{Backoff, RequestBudget, SourcePolicy};

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn unique_tag() -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{now:x}-{counter:x}")
    }
}
