//! User-triggered remote source search UI and worker bridge.
//!
//! Searching or importing only happens after a button click. Workers return
//! messages to `SymtropyApp`, which applies them outside the viewer's snapshot
//! so results cannot be overwritten by the local UI copies.

use crate::gallery::sidecar::Source;
use crate::gallery::Sidecar;
use crate::local_vision::LocalVisionJob;
use crate::tag_review::{ReviewUiState, TagReviewStore};
use egui::{pos2, vec2, Color32, Rect, RichText, Sense, TextEdit, Ui};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver};
use symtropy_sources::{
    download_and_ingest, proposal_for_match, run_lookup, source_search_tags, HttpFetcher,
    IngestResult, LookupReport, LookupRequest, RemoteItem, ScoredCandidate, SourcePolicy,
    SourceSite, DEFAULT_USER_AGENT, validate_source_url,
};
use symtropy_tags::{ApplyPolicy, MatchEvidence};

#[derive(Clone, Debug)]
pub struct SourceSearchJob {
    pub item_id: String,
    pub path: String,
    pub site: SourceSite,
    pub tags: Vec<String>,
    pub exact_url: String,
}

#[derive(Clone, Debug)]
pub struct SourceImportJob {
    pub item_id: String,
    pub item: RemoteItem,
}

#[derive(Clone, Debug, Default)]
pub struct ReviewOutcome {
    pub synced: bool,
    /// A sidecar tag was explicitly preserved or restored from the index.
    pub preservation_changed: bool,
    pub source_attached: bool,
    pub source_search: Option<SourceSearchJob>,
    pub source_import: Option<SourceImportJob>,
    pub local_vision: Option<LocalVisionJob>,
}

#[derive(Debug)]
pub enum SourceWorkerMsg {
    Search {
        item_id: String,
        result: Result<LookupReport, String>,
    },
    Import {
        item_id: String,
        item: RemoteItem,
        result: Result<IngestResult, String>,
    },
}

pub fn spawn_source_search(ctx: &egui::Context, job: SourceSearchJob) -> Receiver<SourceWorkerMsg> {
    let (tx, rx) = channel();
    let worker_ctx = ctx.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| {
            let fetcher = HttpFetcher::for_site(job.site, DEFAULT_USER_AGENT);
            let policy = SourcePolicy::default();
            let Some(adapter) = symtropy_sources::adapter_for(job.site) else {
                return Err(format!("{} adapter is unavailable", job.site.as_str()));
            };
            run_lookup(
                adapter.as_ref(),
                &fetcher,
                &LookupRequest {
                    item_id: job.item_id.clone(),
                    local_path: std::path::PathBuf::from(&job.path),
                    site: job.site,
                    tags: job.tags,
                    exact_url: job.exact_url,
                },
                &policy,
            )
        });
        let result = result.unwrap_or_else(|_| Err("source search worker panicked".to_string()));
        let _ = tx.send(SourceWorkerMsg::Search {
            item_id: job.item_id,
            result,
        });
        worker_ctx.request_repaint();
    });
    rx
}

pub fn spawn_source_import(
    ctx: &egui::Context,
    job: SourceImportJob,
) -> Receiver<SourceWorkerMsg> {
    let (tx, rx) = channel();
    let worker_ctx = ctx.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| {
            let fetcher = HttpFetcher::for_site(job.item.site, DEFAULT_USER_AGENT);
            let ingestor = symtropy_sources::Ingestor::new(crate::paths::cache_dir());
            download_and_ingest(&job.item, &fetcher, &ingestor)
        });
        let result = result.unwrap_or_else(|_| Err("source import worker panicked".to_string()));
        let _ = tx.send(SourceWorkerMsg::Import {
            item_id: job.item_id,
            item: job.item,
            result,
        });
        worker_ctx.request_repaint();
    });
    rx
}

/// Stage a patch built from the current store revision. The worker cannot know
/// the revision and its proposal is intentionally not trusted here; rebuilding
/// it keeps strict-revision checks meaningful.
fn stage_source_evidence(
    review: &mut TagReviewStore,
    item_id: &str,
    evidence: &MatchEvidence,
) -> (usize, usize) {
    if !evidence.verdict.is_usable() {
        return (0, 0);
    }
    let proposal = proposal_for_match(item_id, evidence, review.store.revision());
    match review.store.apply_proposal(&proposal, &ApplyPolicy::conservative()) {
        Ok(report) => {
            let applied = report.applied.len();
            if applied > 0 {
                review.save();
            }
            (applied, report.rejected.len())
        }
        Err(error) => {
            eprintln!("source match: proposal rejected: {error}");
            (0, 1)
        }
    }
}

/// Display candidates only. Staging is a separate, explicit user decision so
/// a lookup never writes remote claims into the review store by itself.
pub fn apply_search_result(
    state: &mut ReviewUiState,
    item_id: &str,
    report: LookupReport,
) {
    let usable = report.candidates.iter().filter(|candidate| candidate.is_usable_match()).count();
    state.source_candidates = report.candidates;
    state.source_previews.clear();
    state.source_busy = false;
    let failures = if report.failures.is_empty() {
        String::new()
    } else {
        format!("; {} preview(s) failed", report.failures.len())
    };
    let truncated = if report.truncated { "; request budget reached" } else { "" };
    state.source_status = format!(
        "{item_id}: {} candidate(s), {} visual match(es) ready for review{}{}",
        state.source_candidates.len(),
        usable,
        failures,
        truncated
    );
}

pub fn apply_import_result(
    state: &mut ReviewUiState,
    staged_sidecar: &mut Option<Sidecar>,
    remote: &RemoteItem,
    result: Result<IngestResult, String>,
) -> bool {
    state.source_busy = false;
    match result {
        Ok(ingest) => {
            if let Some(sidecar) = staged_sidecar.as_mut() {
                if !sidecar.sources.iter().any(|source| source.url == remote.source_url) {
                    sidecar.sources.push(Source {
                        url: remote.source_url.clone(),
                        site: remote.site.as_str().to_string(),
                        captured_at: remote.captured_at.clone(),
                        title: remote.title.clone(),
                        description: remote.description.clone(),
                    });
                }
                sidecar.properties.insert(
                    "source_cache_path".to_string(),
                    ingest.image_path.to_string_lossy().into_owned(),
                );
            }
            state.source_status = format!(
                "Cached {} from {}",
                remote.remote_id,
                remote.site.as_str()
            );
            true
        }
        Err(error) => {
            state.source_status = format!("Import failed: {error}");
            false
        }
    }
}

/// Attach pasted provenance without fetching or scoring anything. This is a
/// manual editor: the URL is the user's claim, not a verified source result.
pub fn attach_manual_source(
    state: &mut ReviewUiState,
    staged_sidecar: &mut Option<Sidecar>,
    metadata_path: Option<&Path>,
) -> Result<(), String> {
    let url = state.attach_url.trim().to_string();
    validate_source_url(&url)?;
    let sidecar = staged_sidecar.as_mut().ok_or_else(|| "No sidecar loaded".to_string())?;
    if sidecar.sources.iter().any(|source| source.url == url) {
        return Err("That source is already attached".to_string());
    }
    let site = if state.attach_site.trim().is_empty() {
        SourceSite::from_url(&url)
            .map(|site| site.as_str().to_string())
            .unwrap_or_else(|| "manual".to_string())
    } else {
        state.attach_site.trim().to_string()
    };
    sidecar.sources.push(Source {
        url,
        site,
        captured_at: String::new(),
        title: state.attach_title.trim().to_string(),
        description: String::new(),
    });
    if let Some(path) = metadata_path {
        sidecar.save(path).map_err(|error| error.to_string())?;
    }
    state.attach_url.clear();
    state.attach_site.clear();
    state.attach_title.clear();
    Ok(())
}

pub fn remove_attached_source(
    staged_sidecar: &mut Option<Sidecar>,
    metadata_path: Option<&Path>,
    index: usize,
) -> Result<(), String> {
    let sidecar = staged_sidecar.as_mut().ok_or_else(|| "No sidecar loaded".to_string())?;
    if index >= sidecar.sources.len() {
        return Err("Source is no longer attached".to_string());
    }
    sidecar.sources.remove(index);
    if let Some(path) = metadata_path {
        sidecar.save(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Compact source editor shown above the remote match panel. Attaching a link
/// never opens a connection; it only records provenance in the staged sidecar.
pub fn draw_source_editor(
    ui: &mut Ui,
    state: &mut ReviewUiState,
    staged_sidecar: &mut Option<Sidecar>,
    metadata_path: Option<&Path>,
    outcome: &mut ReviewOutcome,
) {
    ui.label(RichText::new("ATTACH SOURCE").strong());
    ui.label(
        RichText::new("Paste a link to record provenance. No network request is made.")
            .size(10.0)
            .color(Color32::GRAY),
    );
    ui.horizontal(|ui| {
        ui.label("URL");
        ui.add_sized(
            [ui.available_width() - 32.0, 20.0],
            TextEdit::singleline(&mut state.attach_url).hint_text("https://example.invalid/post/1"),
        );
    });
    ui.horizontal(|ui| {
        ui.label("Site");
        ui.add_sized([100.0, 20.0], TextEdit::singleline(&mut state.attach_site).hint_text("auto"));
        ui.label("Title");
        ui.add_sized(
            [(ui.available_width() - 160.0).max(80.0), 20.0],
            TextEdit::singleline(&mut state.attach_title).hint_text("optional"),
        );
    });
    if ui.button("Attach to sidecar").clicked() {
        match attach_manual_source(state, staged_sidecar, metadata_path) {
            Ok(()) => {
                state.attach_status = "Source attached to sidecar".to_string();
                outcome.source_attached = true;
            }
            Err(error) => state.attach_status = format!("Attach failed: {error}"),
        }
    }
    if !state.attach_status.is_empty() {
        ui.label(RichText::new(&state.attach_status).size(10.0).color(Color32::LIGHT_GRAY));
    }

    let sources = staged_sidecar
        .as_ref()
        .map(|sidecar| sidecar.sources.clone())
        .unwrap_or_default();
    if sources.is_empty() {
        return;
    }
    ui.add_space(4.0);
    ui.label(RichText::new("Attached sources").size(11.0).strong());
    for (index, source) in sources.iter().enumerate() {
        ui.push_id(format!("attached-source:{index}"), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(short_url(&source.url)).monospace());
                ui.label(RichText::new(&source.site).size(10.0).color(Color32::GRAY));
                if ui.small_button("Remove").clicked() {
                    match remove_attached_source(staged_sidecar, metadata_path, index) {
                        Ok(()) => {
                            state.attach_status = "Source removed from sidecar".to_string();
                            outcome.source_attached = true;
                        }
                        Err(error) => state.attach_status = format!("Remove failed: {error}"),
                    }
                }
            });
            if !source.title.is_empty() {
                ui.label(RichText::new(&source.title).size(10.0).color(Color32::from_gray(170)));
            }
        });
    }
}

fn preview_texture(
    ctx: &egui::Context,
    state: &mut ReviewUiState,
    candidate: &ScoredCandidate,
) -> Option<egui::TextureHandle> {
    let key = format!("{}:{}", candidate.item.site.as_str(), candidate.item.remote_id);
    if let Some(handle) = state.source_previews.get(&key) {
        return Some(handle.clone());
    }
    let decoded = image::load_from_memory(&candidate.preview_bytes).ok()?.to_rgba8();
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        decoded.as_raw(),
    );
    let handle = ctx.load_texture(
        format!("source-preview:{key}"),
        image,
        egui::TextureOptions::LINEAR,
    );
    state.source_previews.insert(key, handle.clone());
    Some(handle)
}

fn short_url(url: &str) -> String {
    if url.chars().count() <= 52 {
        return url.to_string();
    }
    let head: String = url.chars().take(38).collect();
    let tail: String = url.chars().rev().take(10).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{head}…{tail}")
}

pub fn draw_source_search(
    ui: &mut Ui,
    review: &mut TagReviewStore,
    state: &mut ReviewUiState,
    item_id: &str,
    sidecar: Option<&Sidecar>,
    outcome: &mut ReviewOutcome,
) {
    ui.label(RichText::new("SOURCE MATCH").strong());
    ui.label(
        RichText::new("Local tags search a site; only visually matched posts suggest tags.")
            .size(10.0)
            .color(Color32::GRAY),
    );
    if !state.source_status.is_empty() {
        ui.label(
            RichText::new(&state.source_status)
                .size(10.0)
                .color(Color32::LIGHT_GRAY),
        );
    }

    ui.horizontal(|ui| {
        egui::ComboBox::from_label("Site")
            .selected_text(state.source_site.as_str())
            .show_ui(ui, |ui| {
                for site in SourceSite::all() {
                    ui.selectable_value(&mut state.source_site, site, site.as_str());
                }
            });
    });
    let capabilities = symtropy_sources::adapter_for(state.source_site)
        .map(|adapter| adapter.capabilities());
    if let Some(capabilities) = &capabilities {
        ui.label(
            RichText::new(&capabilities.notes)
                .size(10.0)
                .color(Color32::GRAY),
        );
    }
    ui.horizontal(|ui| {
        ui.label("URL");
        ui.add_sized(
            [ui.available_width() - 32.0, 20.0],
            TextEdit::singleline(&mut state.source_url)
                .hint_text("paste a post URL, or leave blank to search tags"),
        );
    });
    let path_exists = Path::new(item_id).is_file();
    let has_url = !state.source_url.trim().is_empty();
    let adapter_ready = capabilities.as_ref().is_some_and(|capabilities| {
        capabilities.ready
            && if has_url {
                capabilities.paste_url && !capabilities.requires_api_key
            } else {
                capabilities.search_by_tags
            }
    });
    let enabled = !state.source_busy
        && outcome.source_search.is_none()
        && path_exists
        && adapter_ready;
    if ui.add_enabled(enabled, egui::Button::new("Find matching post")).clicked() {
        let mut tags = sidecar
            .map(|sidecar| {
                sidecar
                    .tags
                    .iter()
                    .cloned()
                    .chain(
                        sidecar
                            .artists
                            .iter()
                            .map(|artist| symtropy_tags::artist_tag_id(artist)),
                    )
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        tags = source_search_tags(&tags, state.source_site);
        outcome.source_search = Some(SourceSearchJob {
            item_id: item_id.to_string(),
            path: item_id.to_string(),
            site: state.source_site,
            tags,
            exact_url: state.source_url.clone(),
        });
        state.source_busy = true;
        state.source_status = "Searching…".to_string();
        state.source_candidates.clear();
        state.source_previews.clear();
    }
    if !path_exists {
        ui.label(
            RichText::new("Open a local image before searching.")
                .size(10.0)
                .color(Color32::GRAY),
        );
    } else if !adapter_ready {
        ui.label(
            RichText::new("This source adapter is not ready for that action.")
                .size(10.0)
                .color(Color32::GRAY),
        );
    }
    ui.separator();

    if state.source_candidates.is_empty() {
        ui.label(
            RichText::new("No candidates yet.")
                .size(11.0)
                .color(Color32::GRAY),
        );
        return;
    }

    let candidates = state.source_candidates.clone();
    for (index, candidate) in candidates.iter().enumerate() {
        ui.push_id(format!("source-candidate:{index}"), |ui| {
            draw_candidate(ui, review, state, item_id, index, candidate, outcome);
        });
        ui.separator();
    }
}

fn draw_candidate(
    ui: &mut Ui,
    review: &mut TagReviewStore,
    state: &mut ReviewUiState,
    item_id: &str,
    index: usize,
    candidate: &ScoredCandidate,
    outcome: &mut ReviewOutcome,
) {
    let score = candidate.similarity();
    if let Some(handle) = preview_texture(ui.ctx(), state, candidate) {
        let [width, height] = handle.size();
        let longest = width.max(height) as f32;
        let scale = (112.0 / longest).min(1.0);
        let display = vec2(width as f32 * scale, height as f32 * scale);
        let (rect, _) = ui.allocate_exact_size(display, Sense::hover());
        ui.painter().image(
            handle.id(),
            rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("{:.0}%", score * 100.0))
                .strong()
                .color(if candidate.is_usable_match() {
                    Color32::from_rgb(120, 220, 140)
                } else {
                    Color32::from_rgb(240, 120, 120)
                }),
        );
        ui.label(RichText::new(format!(
            "{} #{}",
            candidate.item.site.as_str(),
            candidate.item.remote_id
        )));
        if let Some(evidence) = &candidate.evidence {
            ui.label(
                RichText::new(format!("{:?}", evidence.verdict))
                    .size(10.0)
                    .color(Color32::GRAY),
            );
        }
    });
    if !candidate.item.source_url.is_empty() {
        ui.label(
            RichText::new(short_url(&candidate.item.source_url))
                .size(10.0)
                .color(Color32::from_gray(170)),
        );
    }
    let tags = candidate.item.tag_names();
    if !tags.is_empty() {
        ui.label(
            RichText::new(format!("source tags: {}", tags.join(", ")))
                .size(10.0)
                .color(Color32::GRAY),
        );
    }

    ui.horizontal(|ui| {
        let can_stage = candidate.evidence.as_ref().is_some_and(|evidence| evidence.verdict.is_usable());
        if ui.add_enabled(can_stage, egui::Button::new("Stage suggestions")).clicked() {
            if let Some(evidence) = &candidate.evidence {
                let (applied, rejected) = stage_source_evidence(review, item_id, evidence);
                state.source_status = format!(
                    "Staged {applied} suggestion(s) from candidate {index}; {rejected} blocked"
                );
            }
        }
        let can_import = !candidate.item.download_url.is_empty()
            && !state.source_busy
            && outcome.source_import.is_none();
        if ui
            .add_enabled(can_import, egui::Button::new("Import image"))
            .clicked()
        {
            outcome.source_import = Some(SourceImportJob {
                item_id: item_id.to_string(),
                item: candidate.item.clone(),
            });
            state.source_busy = true;
            state.source_status = "Downloading source image…".to_string();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use symtropy_tags::{AssertionState, MatchVerdict, TagStore};

    fn evidence(verdict: MatchVerdict) -> MatchEvidence {
        MatchEvidence {
            remote_id: "123".to_string(),
            source_url: "https://e621.net/posts/123".to_string(),
            site: "e621".to_string(),
            similarity: if verdict.is_usable() { 1.0 } else { 0.1 },
            verdict,
            method: "dhash".to_string(),
            observed_tags: vec!["possession".to_string()],
            local_dimensions: None,
            remote_dimensions: None,
        }
    }

    #[test]
    fn lookup_results_are_not_written_into_the_review_store() {
        let mut state = ReviewUiState::default();
        let report = LookupReport {
            candidates: vec![ScoredCandidate {
                item: RemoteItem::default(),
                evidence: Some(evidence(MatchVerdict::Confirmed)),
                proposal: None,
                preview_bytes: Vec::new(),
            }],
            ..LookupReport::default()
        };

        apply_search_result(&mut state, "local.png", report);

        assert_eq!(state.source_candidates.len(), 1);
        assert!(state.source_status.contains("ready for review"));
        assert!(state.source_previews.is_empty());
    }

    #[test]
    fn staging_a_match_creates_only_namespaced_suggestions() {
        let root = std::env::temp_dir().join(format!(
            "symtropy-source-stage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut review = TagReviewStore::with_store_path(
            TagStore::default(),
            root.join("accepted.json5"),
        );

        let (applied, rejected) =
            stage_source_evidence(&mut review, "local.png", &evidence(MatchVerdict::Confirmed));

        assert_eq!(rejected, 0);
        assert!(applied >= 2);
        let item = review.store.item("local.png").expect("staged item");
        let assertion = item
            .assertion("src:e621:possession")
            .expect("namespaced source suggestion");
        assert_eq!(assertion.state, AssertionState::Suggested);
        assert_eq!(assertion.provenance.kind, symtropy_tags::ProvenanceKind::Source);

        let mut rejected_review = TagReviewStore::with_store_path(
            TagStore::default(),
            root.join("rejected.json5"),
        );
        assert_eq!(
            stage_source_evidence(
                &mut rejected_review,
                "local.png",
                &evidence(MatchVerdict::Rejected)
            ),
            (0, 0)
        );
        assert!(rejected_review.store.item("local.png").is_none());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn staging_a_match_does_not_touch_accepted_or_locked_claims() {
        let root = std::env::temp_dir().join(format!(
            "symtropy-source-claims-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut review = TagReviewStore::with_store_path(
            TagStore::default(),
            root.join("claims.json5"),
        );
        let mut evidence = evidence(MatchVerdict::Confirmed);
        evidence.observed_tags.push("tail".to_string());

        let item = review.store.item_mut("local.png");
        item.upsert_assertion(
            symtropy_tags::TagAssertion::accepted(
                "src:e621:possession",
                symtropy_tags::Provenance::human(),
            )
            .with_evidence("human decision"),
        );
        item.upsert_assertion(
            symtropy_tags::TagAssertion::suggested(
                "src:e621:tail",
                symtropy_tags::Provenance::source("https://e621.net/posts/123"),
                0.5,
            )
            .locked(true),
        );

        let (_, rejected) = stage_source_evidence(&mut review, "local.png", &evidence);
        assert!(rejected >= 2);

        let item = review.store.item("local.png").expect("existing item");
        let possession = item.assertion("src:e621:possession").expect("accepted claim");
        assert_eq!(possession.state, AssertionState::Accepted);
        assert_eq!(possession.provenance.kind, symtropy_tags::ProvenanceKind::Human);
        let tail = item.assertion("src:e621:tail").expect("locked claim");
        assert_eq!(tail.state, AssertionState::Suggested);
        assert!(tail.locked);
        assert_eq!(tail.provenance.kind, symtropy_tags::ProvenanceKind::Source);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn import_result_records_source_provenance_once() {
        let mut state = ReviewUiState::default();
        let mut staged = Some(Sidecar::default());
        let remote = RemoteItem {
            site: SourceSite::E621,
            remote_id: "123".to_string(),
            source_url: "https://e621.net/posts/123".to_string(),
            title: "Example".to_string(),
            ..RemoteItem::default()
        };
        let ingest = IngestResult {
            image_path: PathBuf::from("/cache/sources/e621/123.webp"),
            sidecar_path: PathBuf::from("/cache/sources/e621/123.json"),
            wrote_image: true,
            wrote_sidecar: true,
        };

        assert!(apply_import_result(&mut state, &mut staged, &remote, Ok(ingest)));
        assert!(apply_import_result(
            &mut state,
            &mut staged,
            &remote,
            Ok(IngestResult {
                image_path: PathBuf::from("/cache/sources/e621/123.webp"),
                sidecar_path: PathBuf::from("/cache/sources/e621/123.json"),
                wrote_image: false,
                wrote_sidecar: false,
            }),
        ));

        let sidecar = staged.unwrap();
        assert_eq!(sidecar.sources.len(), 1);
        assert_eq!(sidecar.sources[0].url, remote.source_url);
        assert_eq!(sidecar.sources[0].site, "e621");
        assert_eq!(
            sidecar.properties.get("source_cache_path").map(String::as_str),
            Some("/cache/sources/e621/123.webp")
        );
        assert!(!state.source_busy);
    }

    #[test]
    fn failed_import_keeps_the_error_status() {
        let mut state = ReviewUiState::default();
        let mut staged = Some(Sidecar::default());
        let remote = RemoteItem::default();
        assert!(!apply_import_result(
            &mut state,
            &mut staged,
            &remote,
            Err("network unavailable".to_string())
        ));
        assert!(state.source_status.contains("network unavailable"));
        assert!(!state.source_busy);
    }

    #[test]
    fn manual_source_attach_saves_provenance_and_dedupes() {
        let root = std::env::temp_dir().join(format!(
            "symtropy-attach-source-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let image_path = root.join("local.png");
        let mut state = ReviewUiState::default();
        state.attach_url = "  https://e621.net/posts/123  ".to_string();
        state.attach_title = " Example post ".to_string();
        let mut sidecar = Some(Sidecar::default());

        attach_manual_source(&mut state, &mut sidecar, Some(&image_path)).unwrap();

        let sources = &sidecar.as_ref().unwrap().sources;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].url, "https://e621.net/posts/123");
        assert_eq!(sources[0].site, "e621");
        assert_eq!(sources[0].title, "Example post");
        assert!(Sidecar::path_for(&image_path).exists());
        assert_eq!(Sidecar::load(&image_path).unwrap().sources[0].site, "e621");
        assert!(state.attach_url.is_empty());
        assert!(state.attach_title.is_empty());

        state.attach_url = "https://e621.net/posts/123".to_string();
        let error = attach_manual_source(&mut state, &mut sidecar, Some(&image_path)).unwrap_err();
        assert!(error.contains("already attached"));
        assert_eq!(sidecar.as_ref().unwrap().sources.len(), 1);

        remove_attached_source(&mut sidecar, Some(&image_path), 0).unwrap();
        assert!(sidecar.as_ref().unwrap().sources.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn manual_source_attach_rejects_non_http_urls() {
        let mut state = ReviewUiState::default();
        state.attach_url = "ftp://example.invalid/post/1".to_string();
        let mut sidecar = Some(Sidecar::default());

        let error = attach_manual_source(&mut state, &mut sidecar, None).unwrap_err();

        assert!(error.contains("refusing non-http source URL"));
        assert!(sidecar.as_ref().unwrap().sources.is_empty());
        assert_eq!(state.attach_url, "ftp://example.invalid/post/1");
    }

    #[test]
    fn manual_source_attach_marks_unknown_sites_as_manual() {
        let mut state = ReviewUiState::default();
        state.attach_url = "https://example.invalid/post/1".to_string();
        let mut sidecar = Some(Sidecar::default());

        attach_manual_source(&mut state, &mut sidecar, None).unwrap();

        assert_eq!(sidecar.as_ref().unwrap().sources[0].site, "manual");
    }

    #[test]
    fn long_urls_are_shortened_with_both_ends() {
        let shortened = short_url("https://example.invalid/very/long/path/to/a/post/1234567890");
        assert!(shortened.starts_with("https://example.invalid"));
        assert!(shortened.ends_with("1234567890"));
        assert!(shortened.contains('…'));
    }
}
