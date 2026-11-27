//! Source-independent remote item model.
//!
//! Adapters return these values and never write to disk. Ingest and the tag
//! store decide what becomes a local cache entry or a metadata assertion.

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SourceSite {
    Met,
    Openverse,
    E621,
    Tumblr,
    FurAffinity,
}

impl Default for SourceSite {
    fn default() -> Self {
        Self::E621
    }
}

impl SourceSite {
    pub fn all() -> Vec<Self> {
        vec![Self::Met, Self::Openverse, Self::E621, Self::Tumblr, Self::FurAffinity]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Met => "met",
            Self::Openverse => "openverse",
            Self::E621 => "e621",
            Self::Tumblr => "tumblr",
            Self::FurAffinity => "fur_affinity",
        }
    }

    /// Best-effort local site detection for pasted URLs. Unknown hosts return
    /// `None`; the caller may still attach them as manual provenance.
    pub fn from_url(url: &str) -> Option<Self> {
        let lower = url.to_ascii_lowercase();
        let rest = lower
            .strip_prefix("https://")
            .or_else(|| lower.strip_prefix("http://"))?;
        let host = rest.split(['/', '?', '#']).next()?;
        let host = host.strip_prefix("www.").unwrap_or(host);
        match host {
            "e621.net" | "e926.net" => Some(Self::E621),
            "metmuseum.org" | "www.metmuseum.org" => Some(Self::Met),
            "openverse.org" | "api.openverse.org" => Some(Self::Openverse),
            "tumblr.com" | "www.tumblr.com" => Some(Self::Tumblr),
            "furaffinity.net" | "www.furaffinity.net" => Some(Self::FurAffinity),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RemoteTag {
    pub name: String,
    #[serde(default)] pub category: String,
}

impl RemoteTag {
    pub fn new(name: impl Into<String>, category: impl Into<String>) -> Self {
        Self { name: name.into(), category: category.into() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RemoteItem {
    pub site: SourceSite,
    pub remote_id: String,
    pub source_url: String,
    #[serde(default)] pub preview_url: String,
    #[serde(default)] pub download_url: String,
    #[serde(default)] pub title: String,
    #[serde(default)] pub author: String,
    #[serde(default)] pub description: String,
    #[serde(default)] pub tags: Vec<RemoteTag>,
    #[serde(default)] pub width: Option<u32>,
    #[serde(default)] pub height: Option<u32>,
    #[serde(default)] pub captured_at: String,
}

impl RemoteItem {
    pub fn tag_names(&self) -> Vec<String> {
        self.tags.iter().map(|tag| tag.name.clone()).collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SearchQuery {
    pub site: SourceSite,
    pub tags: Vec<String>,
    #[serde(default = "default_limit")] pub limit: usize,
    /// An exact pasted URL bypasses tag search and goes straight to one item.
    #[serde(default)] pub exact_url: String,
}

fn default_limit() -> usize {
    20
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct FetchRequest {
    pub url: String,
    #[serde(default)] pub user_agent: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct FetchResponse {
    pub bytes: Vec<u8>,
    pub final_url: String,
}

/// Injected network boundary. Production callers supply an HTTP client; tests
/// can supply recorded responses without touching the network.
pub trait Fetcher: Send + Sync {
    fn get(&self, request: &FetchRequest) -> Result<FetchResponse>;
}

#[derive(Clone, Debug, Default)]
pub struct AdapterCapabilities {
    /// False for explicit stubs, so the UI can avoid offering a dead action.
    pub ready: bool,
    pub search_by_tags: bool,
    pub paste_url: bool,
    pub requires_api_key: bool,
    pub notes: String,
}

pub trait SourceAdapter: Send + Sync {
    fn site(&self) -> SourceSite;
    fn capabilities(&self) -> AdapterCapabilities;
    fn search(&self, query: &SearchQuery, fetcher: &dyn Fetcher) -> Result<Vec<RemoteItem>>;
    fn from_url(&self, url: &str, fetcher: &dyn Fetcher) -> Result<RemoteItem>;
}

pub(crate) fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_urls_resolve_to_known_sites() {
        assert_eq!(SourceSite::from_url("https://e621.net/posts/123"), Some(SourceSite::E621));
        assert_eq!(SourceSite::from_url("http://www.tumblr.com/post/1"), Some(SourceSite::Tumblr));
        assert_eq!(SourceSite::from_url("https://example.invalid/post/1"), None);
    }
}
