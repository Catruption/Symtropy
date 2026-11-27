//! User-initiated lookup orchestration.
//!
//! This is the bridge between local metadata and remote candidates. It never
//! schedules itself, never writes to disk, and never turns a remote claim into
//! a local fact. Downloading and ingest are separate, explicit calls.

use crate::http::DEFAULT_USER_AGENT;
use crate::ingest::{IngestResult, Ingestor};
use crate::matching::{evaluate_candidate, proposal_for_match};
use crate::model::{
    FetchRequest, Fetcher, RemoteItem, Result, SearchQuery, SourceAdapter, SourceSite,
};
use crate::policy::{RequestBudget, SourcePolicy};
use std::path::{Path, PathBuf};
use symtropy_tags::{MatchEvidence, Proposal};

#[derive(Clone, Debug, Default)]
pub struct LookupRequest {
    pub item_id: String,
    pub local_path: PathBuf,
    pub site: SourceSite,
    /// Accepted local tags/artist names from the sidecar.
    pub tags: Vec<String>,
    /// A pasted post URL bypasses tag search.
    pub exact_url: String,
}

#[derive(Clone, Debug)]
pub struct ScoredCandidate {
    pub item: RemoteItem,
    /// `None` means the preview could not be evaluated.
    pub evidence: Option<MatchEvidence>,
    /// Headless convenience patch based on revision zero. UI callers should
    /// regenerate it against their current store revision before applying it.
    pub proposal: Option<Proposal>,
    /// Kept so the UI can show the same pixels that were matched.
    pub preview_bytes: Vec<u8>,
}

impl ScoredCandidate {
    pub fn similarity(&self) -> f32 {
        self.evidence.as_ref().map_or(0.0, |evidence| evidence.similarity)
    }

    pub fn is_usable_match(&self) -> bool {
        self.evidence
            .as_ref()
            .is_some_and(|evidence| evidence.verdict.is_usable())
    }
}

#[derive(Clone, Debug, Default)]
pub struct LookupReport {
    pub candidates: Vec<ScoredCandidate>,
    pub requests_used: usize,
    pub truncated: bool,
    pub failures: Vec<String>,
}

/// Convert local sidecar tags into search terms for one site.
///
/// Namespaced evidence from another site is dropped. Local-only semantic IDs
/// are dropped because source APIs do not understand them. Plain tags and
/// `artist:` tags are sent verbatim.
pub fn source_search_tags(tags: &[String], site: SourceSite) -> Vec<String> {
    let site_prefix = format!("src:{}:", site.as_str());
    let mut out: Vec<String> = Vec::new();
    for raw in tags {
        let tag = raw.trim();
        if tag.is_empty() {
            continue;
        }
        let lowered = tag.to_ascii_lowercase();
        if lowered.starts_with("src:") && !lowered.starts_with(&site_prefix.to_ascii_lowercase()) {
            continue;
        }
        let mapped = tag.strip_prefix(&site_prefix).unwrap_or(tag).trim();
        let mapped_lower = mapped.to_ascii_lowercase();
        if mapped.is_empty()
            || ["act:", "state:", "theme:", "sym:", "feature:", "rel:"]
                .iter()
                .any(|prefix| mapped_lower.starts_with(prefix))
        {
            continue;
        }
        if !out.iter().any(|existing| existing.eq_ignore_ascii_case(mapped)) {
            out.push(mapped.to_string());
            if out.len() == 6 {
                break;
            }
        }
    }
    out
}

/// Run one user-initiated search and evaluate every returned preview.
///
/// One request is spent on search, then one per preview. The shared policy
/// stops the run at `max_requests_per_run`, so a single click cannot become a
/// crawl.
pub fn run_lookup(
    adapter: &dyn SourceAdapter,
    fetcher: &dyn Fetcher,
    request: &LookupRequest,
    policy: &SourcePolicy,
) -> Result<LookupReport> {
    if !policy.allows(request.site) {
        return Err(format!("{} is not an allowed source site", request.site.as_str()));
    }
    let mut budget = RequestBudget::new();
    let mut report = LookupReport::default();
    let exact_url = request.exact_url.trim().to_string();
    let exact = !exact_url.is_empty();
    let tags = if exact {
        Vec::new()
    } else {
        source_search_tags(&request.tags, request.site)
    };
    let query = SearchQuery {
        site: request.site,
        tags,
        limit: policy.max_results.clamp(1, 320),
        exact_url,
    };

    if !budget.try_spend(policy.max_requests_per_run) {
        return Err("source policy allows no requests".to_string());
    }
    let mut items = adapter.search(&query, fetcher)?;
    if items.len() > policy.max_results {
        items.truncate(policy.max_results);
    }

    for item in items {
        if !budget.try_spend(policy.max_requests_per_run) {
            report.truncated = true;
            break;
        }
        let preview_url = if item.preview_url.trim().is_empty() {
            item.download_url.as_str()
        } else {
            item.preview_url.as_str()
        };
        if preview_url.is_empty() {
            report.failures.push(format!(
                "{} has no preview URL",
                if item.remote_id.is_empty() { "unknown post" } else { &item.remote_id }
            ));
            continue;
        }

        match fetcher.get(&FetchRequest {
            url: preview_url.to_string(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
        }) {
            Ok(response) => {
                let evidence = match evaluate_candidate(
                    Path::new(&request.local_path),
                    &response.bytes,
                    &item,
                    policy,
                ) {
                    Ok(evidence) => evidence,
                    Err(error) => {
                        report.failures.push(format!("{}: {error}", item.remote_id));
                        continue;
                    }
                };
                let proposal = proposal_for_match(&request.item_id, &evidence, 0);
                report.candidates.push(ScoredCandidate {
                    item,
                    evidence: Some(evidence),
                    proposal: Some(proposal),
                    preview_bytes: response.bytes,
                });
            }
            Err(error) => report
                .failures
                .push(format!("{}: {error}", item.remote_id)),
        }
    }

    report.requests_used = budget.used();
    report.candidates.sort_by(|left, right| {
        right
            .similarity()
            .total_cmp(&left.similarity())
            .then_with(|| left.item.remote_id.cmp(&right.item.remote_id))
    });
    Ok(report)
}

/// Download the full source file and materialize it into the local cache.
/// Call this only after the user chose a candidate.
pub fn download_and_ingest(
    item: &RemoteItem,
    fetcher: &dyn Fetcher,
    ingestor: &Ingestor,
) -> Result<IngestResult> {
    let url = item.download_url.trim();
    if url.is_empty() {
        return Err("candidate has no full download URL".to_string());
    }
    let response = fetcher.get(&FetchRequest {
        url: url.to_string(),
        user_agent: DEFAULT_USER_AGENT.to_string(),
    })?;
    ingestor.ingest(item, &response.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::E621Adapter;
    use crate::model::RemoteTag;
    use image::{ImageBuffer, Luma};

    struct FakeFetcher {
        json: Vec<u8>,
        image: Vec<u8>,
    }

    impl Fetcher for FakeFetcher {
        fn get(&self, request: &FetchRequest) -> Result<crate::model::FetchResponse> {
            let bytes = if request.url.contains("posts.json") {
                self.json.clone()
            } else {
                self.image.clone()
            };
            Ok(crate::model::FetchResponse {
                bytes,
                final_url: request.url.clone(),
            })
        }
    }

    fn png(size: u32) -> Vec<u8> {
        let image = ImageBuffer::from_fn(size, size, |x, y| {
            Luma([(((x * 3 + y * 5) % 31) as u8).wrapping_mul(7)])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageLuma8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn local_semantic_ids_are_not_sent_to_source_apis() {
        let tags = vec![
            "src:e621:possession".to_string(),
            "src:other:tail".to_string(),
            "act:possession".to_string(),
            "artist:kitty".to_string(),
            "orange fox".to_string(),
        ];
        assert_eq!(
            source_search_tags(&tags, SourceSite::E621),
            vec!["possession".to_string(), "artist:kitty".to_string(), "orange fox".to_string()]
        );
    }

    #[test]
    fn lookup_matches_a_candidate_and_builds_a_proposal() {
        let root = std::env::temp_dir().join(format!(
            "symtropy-lookup-test-{}-{}",
            std::process::id(),
            crate::tests::unique_tag()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let local_path = root.join("local.png");
        let bytes = png(48);
        std::fs::write(&local_path, &bytes).unwrap();

        let json = br#"{
            "posts": [{
                "id": 123,
                "file": {"url": "https://static.example/123.png", "width": 48, "height": 48},
                "preview": {"url": "https://static.example/preview/123.png"},
                "tags": {"artist": ["kitty"], "general": ["possession"]}
            }]
        }"#;
        let fetcher = FakeFetcher { json: json.to_vec(), image: bytes };
        let report = run_lookup(
            &E621Adapter,
            &fetcher,
            &LookupRequest {
                item_id: "local.png".to_string(),
                local_path: local_path.clone(),
                site: SourceSite::E621,
                tags: vec!["possession".to_string()],
                exact_url: String::new(),
            },
            &SourcePolicy::default(),
        )
        .unwrap();

        assert_eq!(report.failures.len(), 0);
        assert_eq!(report.candidates.len(), 1);
        let candidate = &report.candidates[0];
        assert!(candidate.is_usable_match());
        assert_eq!(candidate.item.download_url, "https://static.example/123.png");
        assert!(candidate.item.preview_url.contains("preview"));
        assert!(candidate
            .proposal
            .as_ref()
            .is_some_and(|proposal| !proposal.operations.is_empty()));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn policy_can_disallow_a_site_before_any_fetch() {
        let fetcher = FakeFetcher { json: Vec::new(), image: png(8) };
        let policy = SourcePolicy { allowed_sites: Vec::new(), ..SourcePolicy::default() };
        let error = run_lookup(
            &E621Adapter,
            &fetcher,
            &LookupRequest {
                site: SourceSite::E621,
                tags: vec!["possession".to_string()],
                ..LookupRequest::default()
            },
            &policy,
        )
        .unwrap_err();
        assert!(error.contains("not an allowed source site"));
    }

    #[test]
    fn import_never_falls_back_to_the_low_resolution_preview() {
        let root = std::env::temp_dir().join(format!(
            "symtropy-preview-fallback-test-{}-{}",
            std::process::id(),
            crate::tests::unique_tag()
        ));
        let fetcher = FakeFetcher { json: Vec::new(), image: b"preview".to_vec() };
        let item = RemoteItem {
            site: SourceSite::E621,
            remote_id: "123".to_string(),
            source_url: "https://e621.net/posts/123".to_string(),
            preview_url: "https://static.example/preview/123.png".to_string(),
            ..RemoteItem::default()
        };
        let error = download_and_ingest(&item, &fetcher, &Ingestor::new(&root)).unwrap_err();
        assert!(error.contains("full download URL"));
        assert!(!root.exists());
    }

    #[test]
    fn download_uses_the_full_source_url_and_writes_a_sidecar() {
        let root = std::env::temp_dir().join(format!(
            "symtropy-download-test-{}-{}",
            std::process::id(),
            crate::tests::unique_tag()
        ));
        let fetcher = FakeFetcher { json: Vec::new(), image: b"full-image".to_vec() };
        let item = RemoteItem {
            site: SourceSite::E621,
            remote_id: "123".to_string(),
            source_url: "https://e621.net/posts/123".to_string(),
            preview_url: "https://static.example/preview/123.png".to_string(),
            download_url: "https://static.example/123.png".to_string(),
            tags: vec![RemoteTag::new("fox", "general")],
            ..RemoteItem::default()
        };
        let result = download_and_ingest(&item, &fetcher, &Ingestor::new(&root)).unwrap();
        assert!(result.wrote_image && result.wrote_sidecar);
        assert_eq!(std::fs::read(&result.image_path).unwrap(), b"full-image");
        let sidecar = std::fs::read_to_string(&result.sidecar_path).unwrap();
        assert!(sidecar.contains("https://e621.net/posts/123"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
