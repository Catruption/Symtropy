//! Per-site adapters. They return metadata and never touch disk.
//!
//! e621 is implemented because it has a public, tag-rich API. Other sites are
//! explicit stubs: they must not fall back to scraping as canonical behavior.

use crate::model::{
    percent_encode, AdapterCapabilities, FetchRequest, Fetcher, RemoteItem,
    RemoteTag, Result, SearchQuery, SourceAdapter, SourceSite,
};
use serde_json::Value;

const USER_AGENT: &str = "Symtropy/0.1 (local-first gallery; user-initiated lookup; contact: local)";

fn json_number(value: Option<&Value>) -> Option<u32> {
    value.and_then(Value::as_u64).and_then(|number| u32::try_from(number).ok())
}

fn json_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

fn e621_tags(post: &Value) -> Vec<RemoteTag> {
    let Some(tags) = post.get("tags") else { return Vec::new() };
    let mut out = Vec::new();
    for category in ["artist", "copyright", "character", "general"] {
        let Some(values) = tags.get(category).and_then(Value::as_array) else { continue };
        for value in values {
            let name = json_text(Some(value));
            if !name.is_empty() {
                out.push(RemoteTag::new(name, category));
            }
        }
    }
    out
}

fn e621_item(post: &Value) -> Option<RemoteItem> {
    let id = json_text(post.get("id"));
    if id.is_empty() {
        return None;
    }
    let file = post.get("file");
    let tags = e621_tags(post);
    let author = tags
        .iter()
        .find(|tag| tag.category == "artist")
        .map(|tag| tag.name.clone())
        .unwrap_or_default();
    Some(RemoteItem {
        site: SourceSite::E621,
        remote_id: id.clone(),
        source_url: format!("https://e621.net/posts/{id}"),
        preview_url: json_text(post.get("preview").and_then(|preview| preview.get("url"))),
        download_url: json_text(file.and_then(|file| file.get("url"))),
        title: String::new(),
        author,
        description: json_text(post.get("description")),
        tags,
        width: json_number(file.and_then(|file| file.get("width"))),
        height: json_number(file.and_then(|file| file.get("height"))),
        captured_at: json_text(post.get("created_at")),
    })
}

pub struct E621Adapter;

impl SourceAdapter for E621Adapter {
    fn site(&self) -> SourceSite {
        SourceSite::E621
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            ready: true,
            search_by_tags: true,
            paste_url: true,
            requires_api_key: false,
            notes: "Public API; keep under 2 requests/second and send a descriptive User-Agent.".into(),
        }
    }

    fn search(&self, query: &SearchQuery, fetcher: &dyn Fetcher) -> Result<Vec<RemoteItem>> {
        if !query.exact_url.is_empty() {
            return Ok(vec![self.from_url(&query.exact_url, fetcher)?]);
        }
        if query.tags.is_empty() {
            return Err("e621 search requires at least one tag".to_string());
        }
        let limit = query.limit.clamp(1, 320);
        let tags = percent_encode(&query.tags.join(" "));
        let request = FetchRequest {
            url: format!("https://e621.net/posts.json?limit={limit}&tags={tags}"),
            user_agent: USER_AGENT.to_string(),
        };
        let response = fetcher.get(&request)?;
        let json: Value = serde_json::from_slice(&response.bytes)
            .map_err(|error| format!("e621 returned invalid JSON: {error}"))?;
        let posts = json
            .get("posts")
            .and_then(Value::as_array)
            .ok_or_else(|| "e621 response has no posts array".to_string())?;
        Ok(posts.iter().filter_map(e621_item).collect())
    }

    fn from_url(&self, url: &str, fetcher: &dyn Fetcher) -> Result<RemoteItem> {
        if !(url.starts_with("https://e621.net/") || url.starts_with("http://e621.net/")) {
            return Err("e621 adapter only accepts e621.net post URLs".to_string());
        }
        let id = url
            .rsplit('/')
            .find(|part| part.chars().all(|ch| ch.is_ascii_digit()) && !part.is_empty())
            .ok_or_else(|| "could not find an e621 post id in that URL".to_string())?;
        let request = FetchRequest {
            url: format!("https://e621.net/posts.json?tags=id%3A{id}"),
            user_agent: USER_AGENT.to_string(),
        };
        let response = fetcher.get(&request)?;
        let json: Value = serde_json::from_slice(&response.bytes)
            .map_err(|error| format!("e621 returned invalid JSON: {error}"))?;
        json.get("posts")
            .and_then(Value::as_array)
            .and_then(|posts| posts.first())
            .and_then(e621_item)
            .ok_or_else(|| format!("e621 post {id} was not found"))
    }
}

macro_rules! stub_adapter {
    ($name:ident, $site:expr, $note:expr, $paste:expr, $key:expr) => {
        pub struct $name;

        impl SourceAdapter for $name {
            fn site(&self) -> SourceSite {
                $site
            }

            fn capabilities(&self) -> AdapterCapabilities {
                AdapterCapabilities {
                    ready: false,
                    search_by_tags: false,
                    paste_url: $paste,
                    requires_api_key: $key,
                    notes: $note.to_string(),
                }
            }

            fn search(&self, _query: &SearchQuery, _fetcher: &dyn Fetcher) -> Result<Vec<RemoteItem>> {
                Err(format!("{} tag search is not enabled; paste an item URL instead", $site.as_str()))
            }

            fn from_url(&self, _url: &str, _fetcher: &dyn Fetcher) -> Result<RemoteItem> {
                Err(format!("{} adapter is not configured yet", $site.as_str()))
            }
        }
    };
}

stub_adapter!(
    MetAdapter,
    SourceSite::Met,
    "Legal seed source. Broad API search needs a curated query plan before it is enabled.",
    false,
    false
);
stub_adapter!(
    OpenverseAdapter,
    SourceSite::Openverse,
    "Legal seed source. Search should be enabled only with an explicit curated query.",
    false,
    false
);
stub_adapter!(
    TumblrAdapter,
    SourceSite::Tumblr,
    "API v2 requires a user-supplied key. Pasted post URLs are preferred.",
    true,
    true
);
stub_adapter!(
    FurAffinityAdapter,
    SourceSite::FurAffinity,
    "Official content API is beta/rolling. No scraping fallback; pasted links only.",
    true,
    false
);

pub fn all_adapters() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(E621Adapter),
        Box::new(MetAdapter),
        Box::new(OpenverseAdapter),
        Box::new(TumblrAdapter),
        Box::new(FurAffinityAdapter),
    ]
}

pub fn adapter_for(site: SourceSite) -> Option<Box<dyn SourceAdapter>> {
    all_adapters().into_iter().find(|adapter| adapter.site() == site)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FetchResponse;

    struct FakeFetcher {
        body: Vec<u8>,
    }

    impl Fetcher for FakeFetcher {
        fn get(&self, _request: &FetchRequest) -> Result<FetchResponse> {
            Ok(FetchResponse {
                bytes: self.body.clone(),
                final_url: "https://e621.net/posts.json".to_string(),
            })
        }
    }

    #[test]
    fn e621_rejects_foreign_pasted_urls() {
        let fetcher = FakeFetcher { body: Vec::new() };
        let error = E621Adapter
            .from_url("https://example.invalid/posts/123", &fetcher)
            .unwrap_err();
        assert!(error.contains("e621.net"));
    }

    #[test]
    fn e621_search_parses_namespaced_candidates() {
        let body = br#"{
            "posts": [{
                "id": 123,
                "file": {"url": "https://static.example/123.png", "width": 800, "height": 600},
                "preview": {"url": "https://static.example/preview/123.png"},
                "description": "a local test post",
                "created_at": "2026-01-02T03:04:05Z",
                "tags": {"artist": ["kitty"], "general": ["possession", "orange"]}
            }]
        }"#;
        let fetcher = FakeFetcher { body: body.to_vec() };
        let items = E621Adapter
            .search(
                &SearchQuery {
                    site: SourceSite::E621,
                    tags: vec!["possession".into(), "artist:kitty".into()],
                    limit: 20,
                    exact_url: String::new(),
                },
                &fetcher,
            )
            .unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].remote_id, "123");
        assert_eq!(items[0].preview_url, "https://static.example/preview/123.png");
        assert_eq!(items[0].download_url, "https://static.example/123.png");
        assert_eq!(items[0].width, Some(800));
        assert_eq!(items[0].tag_names(), vec!["kitty", "possession", "orange"]);
        assert_eq!(items[0].author, "kitty");
        assert_eq!(items[0].description, "a local test post");
        assert_eq!(items[0].captured_at, "2026-01-02T03:04:05Z");
        assert!(items[0].tags.iter().any(|tag| tag.category == "artist"));
    }

    #[test]
    fn registry_covers_expected_sites() {
        let mut sites: Vec<SourceSite> = all_adapters().iter().map(|adapter| adapter.site()).collect();
        let mut expected = SourceSite::all();
        sites.sort();
        expected.sort();
        assert_eq!(sites, expected);
        assert!(adapter_for(SourceSite::E621).unwrap().capabilities().search_by_tags);
        assert!(adapter_for(SourceSite::FurAffinity).unwrap().capabilities().paste_url);
    }
}
