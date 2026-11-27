//! UI bridge between sidecars and the headless tag core.
//!
//! This module does two things:
//!
//! - imports existing sidecar metadata as accepted, provenance-bearing assertions;
//! - lets the user review suggestions and relations before any of them become
//!   authoritative.
//!
//! AI output is never written straight into a sidecar. It lands in the tag
//! store as `Suggested` claims, and only explicit user decisions can promote
//! it or sync the accepted projection back to disk.

use crate::gallery::sidecar::Relationship;
use crate::gallery::Sidecar;
use crate::local_vision::{
    self, LocalSuggestion, LocalVisionJob, OnnxInputLayout, OnnxNormalization,
    OnnxOutputInterpretation,
};
use crate::source_search::{draw_source_editor, draw_source_search, ReviewOutcome};
use egui::{Color32, RichText, ScrollArea, TextEdit, Ui};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use symtropy_sources::{ScoredCandidate, SourceSite};
use symtropy_tags::{
    artist_tag_id, stable_suffix, ApplyPolicy, AssertionState, EntityRef, Operation, Proposal,
    Provenance, RelationPredicate, RelationScope, TagAssertion, TagRelation, TagStore,
};

#[derive(Clone, Default)]
pub struct ReviewUiState {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub global_subject: String,
    pub global_predicate: String,
    pub global_object: String,
    pub status: String,
    pub source_site: SourceSite,
    pub source_url: String,
    pub source_status: String,
    pub source_busy: bool,
    pub source_candidates: Vec<ScoredCandidate>,
    pub source_previews: HashMap<String, egui::TextureHandle>,
    pub attach_url: String,
    pub attach_site: String,
    pub attach_title: String,
    pub attach_status: String,
    /// Explicit local ONNX model settings; never populated from the network.
    pub onnx: local_vision::OnnxUiConfig,
}

pub struct TagReviewStore {
    pub store: TagStore,
    path: PathBuf,
}

impl Default for TagReviewStore {
    fn default() -> Self {
        Self { store: TagStore::default(), path: review_store_path() }
    }
}

fn review_store_path() -> PathBuf {
    crate::paths::config_dir().join("symtropy_tag_store.json5")
}

impl TagReviewStore {
    pub fn load() -> Self {
        let path = review_store_path();
        let store = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| TagStore::from_json5(&text).ok())
            .unwrap_or_default();
        Self { store, path }
    }

    #[cfg(test)]
    pub fn with_store_path(store: TagStore, path: PathBuf) -> Self {
        Self { store, path }
    }

    pub fn save(&self) {
        let Ok(text) = self.store.to_json5_pretty() else {
            eprintln!("tag review: failed to serialize store");
            return;
        };
        if std::fs::read_to_string(&self.path).ok().as_deref() == Some(text.as_str()) {
            return;
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.path.with_extension("json5.tmp");
        if let Err(error) = std::fs::write(&tmp, text) {
            eprintln!("tag review: failed to write {}: {}", tmp.display(), error);
            return;
        }
        if let Err(error) = std::fs::rename(&tmp, &self.path) {
            eprintln!("tag review: failed to replace {}: {}", self.path.display(), error);
        }
    }

    /// Bring existing sidecar metadata into the review store without ever
    /// overwriting a claim that is already there.
    pub fn import_sidecar(&mut self, item_id: &str, sidecar: &Sidecar) -> usize {
        let provenance = Provenance::import("sidecar");
        let existing = self.store.item(item_id);
        let mut planned_tags = BTreeSet::new();
        let mut planned_relations = BTreeSet::new();
        let mut operations = Vec::new();

        let sidecar_tags = sidecar.active_tags();
        let artist_ids: Vec<String> = sidecar
            .artists
            .iter()
            .map(|artist| artist_tag_id(artist))
            .filter(|id| !sidecar.is_preserved_tag(id))
            .collect();
        for tag in sidecar_tags.iter().chain(artist_ids.iter()) {
            let tag = tag.trim();
            if tag.is_empty()
                || planned_tags.contains(tag)
                || existing.and_then(|item| item.assertion(tag)).is_some()
            {
                continue;
            }
            planned_tags.insert(tag.to_string());
            operations.push(Operation::AddAssertion {
                item: item_id.to_string(),
                assertion: TagAssertion::accepted(tag, provenance.clone())
                    .with_evidence("imported from sidecar"),
            });
        }

        for relationship in &sidecar.relationships {
            let id = format!(
                "sidecar-{}-{}-{}",
                stable_suffix(item_id),
                stable_suffix(&relationship.subject),
                stable_suffix(&relationship.object)
            );
            if planned_relations.contains(&id)
                || existing.and_then(|item| item.relation(&id)).is_some()
            {
                continue;
            }
            planned_relations.insert(id.clone());
            operations.push(Operation::AddRelation {
                scope: RelationScope::Item(item_id.to_string()),
                relation: TagRelation::new(
                    id,
                    entity_from_text(item_id, &relationship.subject),
                    RelationPredicate::parse(&relationship.predicate),
                    entity_from_text(item_id, &relationship.object),
                    AssertionState::Accepted,
                    provenance.clone(),
                    1.0,
                )
                .with_evidence("imported from sidecar relationship"),
            });
        }

        if operations.is_empty() {
            return 0;
        }
        let proposal = Proposal {
            id: format!("sidecar-import-{}", stable_suffix(item_id)),
            based_on_revision: self.store.revision(),
            provenance: provenance.clone(),
            operations,
            ..Proposal::default()
        };
        match self.store.apply_proposal(&proposal, &ApplyPolicy::default()) {
            Ok(report) => {
                let count = report.applied.len();
                if count > 0 {
                    self.save();
                }
                count
            }
            Err(error) => {
                eprintln!("tag review: sidecar import failed: {}", error);
                0
            }
        }
    }

    /// Capture source URLs that are already in a local sidecar. These become
    /// suggested `evidences` relations only: they are provenance, not verified
    /// semantic tags, and they never replace an accepted or locked claim.
    pub fn capture_sidecar_sources(&mut self, item_id: &str, sidecar: &Sidecar) -> usize {
        let existing = self.store.item(item_id);
        let mut operations = Vec::new();
        for source in &sidecar.sources {
            let url = source.url.trim();
            if url.is_empty() {
                continue;
            }
            let id = format!(
                "source-{}-{}",
                stable_suffix(item_id),
                stable_suffix(url)
            );
            if existing.and_then(|item| item.relation(&id)).is_some() {
                continue;
            }
            let mut provenance = Provenance::source(url.to_string());
            provenance.tool = "sidecar-source".to_string();
            if source.title.is_empty() {
                provenance.note = format!("source site: {}", source.site);
            } else {
                provenance.note = format!("{} · source site: {}", source.title, source.site);
            }
            operations.push(Operation::AddRelation {
                scope: RelationScope::Item(item_id.to_string()),
                relation: TagRelation::new(
                    id,
                    EntityRef::External(url.to_string()),
                    RelationPredicate::Evidences,
                    EntityRef::Item(item_id.to_string()),
                    AssertionState::Suggested,
                    provenance,
                    0.6,
                )
                .with_evidence("captured from local sidecar source provenance"),
            });
        }
        if operations.is_empty() {
            return 0;
        }
        let proposal = Proposal {
            id: format!("capture-sources-{}", stable_suffix(item_id)),
            based_on_revision: self.store.revision(),
            provenance: Provenance::source("local-sidecar"),
            operations,
            ..Proposal::default()
        };
        match self.store.apply_proposal(&proposal, &ApplyPolicy::conservative()) {
            Ok(report) => {
                let count = report.applied.len();
                if count > 0 {
                    self.save();
                }
                count
            }
            Err(error) => {
                eprintln!("tag review: source capture failed: {}", error);
                0
            }
        }
    }

    /// Ask the headless core to convert artist-looking tags into suggestions.
    /// The original tags are left untouched.
    pub fn suggest_artist_metadata(&mut self, item_id: &str, sidecar: &Sidecar) -> (usize, usize) {
        let proposal = symtropy_tags::propose_artist_conversion(
            item_id,
            &sidecar.tags,
            &sidecar.artists,
            self.store.revision(),
        );
        let preview = self.store.preview(&proposal, &ApplyPolicy::conservative());
        let report = match self.store.apply_proposal(&proposal, &ApplyPolicy::conservative()) {
            Ok(report) => report,
            Err(error) => {
                eprintln!("tag review: artist suggestion failed: {}", error);
                return (0, preview.rejected.len());
            }
        };
        if !report.applied.is_empty() {
            self.save();
        }
        (report.applied.len(), report.rejected.len() + preview.rejected.len())
    }

    /// Stage local feature guesses as reviewable suggestions. The default
    /// policy still protects accepted and locked claims while allowing weak
    /// guesses into review at confidence below 50%.
    pub fn stage_local_suggestions(
        &mut self,
        item_id: &str,
        suggestions: &[LocalSuggestion],
    ) -> (usize, usize) {
        let mut seen = std::collections::BTreeSet::new();
        let operations: Vec<Operation> = suggestions
            .iter()
            .filter(|suggestion| {
                let tag = suggestion.tag.trim();
                !tag.is_empty() && suggestion.confidence < 1.0 && seen.insert(tag.to_string())
            })
            .map(|suggestion| Operation::AddAssertion {
                item: item_id.to_string(),
                assertion: TagAssertion::suggested(
                    &suggestion.tag,
                    Provenance::ai("local-vision", &suggestion.model_version),
                    suggestion.confidence,
                )
                .with_evidence(&suggestion.reason),
            })
            .collect();
        if operations.is_empty() {
            return (0, 0);
        }
        let provenance = Provenance::ai("local-vision", "review-bridge-v1");
        let proposal = Proposal {
            id: format!("local-vision-{}", stable_suffix(item_id)),
            based_on_revision: self.store.revision(),
            provenance: provenance.clone(),
            operations,
            ..Proposal::default()
        };
        let policy = ApplyPolicy::default();
        let preview = self.store.preview(&proposal, &policy);
        let report = match self.store.apply_proposal(&proposal, &policy) {
            Ok(report) => report,
            Err(error) => {
                eprintln!("tag review: local suggestion staging failed: {}", error);
                return (0, preview.rejected.len() + 1);
            }
        };
        if !report.applied.is_empty() {
            self.save();
        }
        (report.applied.len(), report.rejected.len())
    }

    pub fn set_assertion_state(
        &mut self,
        item_id: &str,
        tag: &str,
        state: AssertionState,
    ) -> Result<(), String> {
        let provenance = Provenance::human();
        let proposal = Proposal {
            id: format!("review-assertion-{}-{}", stable_suffix(item_id), stable_suffix(tag)),
            based_on_revision: self.store.revision(),
            provenance: provenance.clone(),
            operations: vec![Operation::SetAssertionState {
                item: item_id.to_string(),
                tag: tag.to_string(),
                state,
                superseded_by: None,
                provenance,
                evidence: vec!["reviewed in Symtropy".to_string()],
            }],
            ..Proposal::default()
        };
        let report = self.store.apply_proposal(&proposal, &ApplyPolicy::default())?;
        if report.applied.is_empty() {
            return Err(report
                .rejected
                .first()
                .map(|rejected| rejected.reason.clone())
                .unwrap_or_else(|| "change was not applied".to_string()));
        }
        self.save();
        Ok(())
    }

    pub fn set_relation_state_scoped(
        &mut self,
        scope: RelationScope,
        relation_id: &str,
        state: AssertionState,
    ) -> Result<(), String> {
        let provenance = Provenance::human();
        let proposal = Proposal {
            id: format!("review-relation-{}-{}", scope.key(), stable_suffix(relation_id)),
            based_on_revision: self.store.revision(),
            provenance: provenance.clone(),
            operations: vec![Operation::SetRelationState {
                scope,
                relation_id: relation_id.to_string(),
                state,
                superseded_by: None,
                provenance,
                evidence: vec!["reviewed in Symtropy".to_string()],
            }],
            ..Proposal::default()
        };
        let report = self.store.apply_proposal(&proposal, &ApplyPolicy::default())?;
        if report.applied.is_empty() {
            return Err(report
                .rejected
                .first()
                .map(|rejected| rejected.reason.clone())
                .unwrap_or_else(|| "change was not applied".to_string()));
        }
        self.save();
        Ok(())
    }

    pub fn toggle_assertion_lock(&mut self, item_id: &str, tag: &str, locked: bool) -> bool {
        let changed = self.store.set_assertion_locked(item_id, tag, locked);
        if changed {
            self.save();
        }
        changed
    }

    pub fn toggle_relation_lock_scoped(
        &mut self,
        scope: RelationScope,
        id: &str,
        locked: bool,
    ) -> bool {
        let changed = self.store.set_relation_locked(&scope, id, locked);
        if changed {
            self.save();
        }
        changed
    }

    pub fn add_relation_scoped(
        &mut self,
        scope: RelationScope,
        id: String,
        subject: EntityRef,
        predicate: RelationPredicate,
        object: EntityRef,
    ) -> Result<(), String> {
        let relation = TagRelation::new(
            id,
            subject,
            predicate,
            object,
            AssertionState::Accepted,
            Provenance::human(),
            1.0,
        );
        let proposal = Proposal {
            id: format!("add-relation-{}-{}", scope.key(), stable_suffix(&relation.id)),
            based_on_revision: self.store.revision(),
            provenance: Provenance::human(),
            operations: vec![Operation::AddRelation {
                scope,
                relation,
            }],
            ..Proposal::default()
        };
        let report = self.store.apply_proposal(&proposal, &ApplyPolicy::default())?;
        if report.applied.is_empty() {
            return Err(report
                .rejected
                .first()
                .map(|rejected| rejected.reason.clone())
                .unwrap_or_else(|| "relation was not added".to_string()));
        }
        self.save();
        Ok(())
    }
}

#[derive(Clone)]
struct PreservationRow {
    tag: String,
    state: Option<AssertionState>,
    preserved: bool,
    sidecar_version: u32,
    superseded_by: Option<String>,
}

fn preservation_rows(
    review: &TagReviewStore,
    item_id: &str,
    staged_sidecar: Option<&Sidecar>,
) -> Vec<PreservationRow> {
    let Some(sidecar) = staged_sidecar else {
        return Vec::new();
    };
    let item = review.store.item(item_id);
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();

    for tag in &sidecar.tags {
        if tag.trim().is_empty() || !seen.insert(tag.clone()) {
            continue;
        }
        let assertion = item.and_then(|item| item.assertion(tag));
        let preserved = sidecar.is_preserved_tag(tag);
        let state = assertion.map(|assertion| assertion.state);
        let significant = preserved
            || state.is_none()
            || matches!(
                state,
                Some(AssertionState::Rejected | AssertionState::Superseded)
            );
        if !significant {
            continue;
        }
        rows.push(PreservationRow {
            tag: tag.clone(),
            state,
            preserved,
            sidecar_version: sidecar.version,
            superseded_by: assertion.and_then(|assertion| assertion.superseded_by.clone()),
        });
    }

    rows.sort_by(|left, right| {
        left.preserved
            .cmp(&right.preserved)
            .then(left.tag.cmp(&right.tag))
    });
    rows
}

#[derive(Clone)]
struct AssertionRow {
    tag: String,
    state: AssertionState,
    confidence: f32,
    provenance: Provenance,
    evidence: Vec<String>,
    locked: bool,
}

fn state_color(state: AssertionState) -> Color32 {
    match state {
        AssertionState::Accepted => Color32::from_rgb(120, 220, 140),
        AssertionState::Suggested => Color32::from_rgb(240, 200, 100),
        AssertionState::Rejected => Color32::from_rgb(240, 120, 120),
        AssertionState::Superseded => Color32::from_gray(140),
    }
}

fn state_label(state: AssertionState) -> &'static str {
    match state {
        AssertionState::Accepted => "ACCEPTED",
        AssertionState::Suggested => "SUGGESTED",
        AssertionState::Rejected => "REJECTED",
        AssertionState::Superseded => "SUPERSEDED",
    }
}

fn provenance_line(provenance: &Provenance) -> String {
    let mut parts = vec![format!("{:?}", provenance.kind)];
    if !provenance.tool.is_empty() {
        parts.push(provenance.tool.clone());
    }
    if !provenance.model_version.is_empty() {
        parts.push(provenance.model_version.clone());
    }
    parts.join(" · ")
}

pub(crate) fn entity_from_text(item_id: &str, text: &str) -> EntityRef {
    let trimmed = text.trim();
    if let Some(value) = trimmed.strip_prefix("item:") {
        EntityRef::Item(value.trim().to_string())
    } else if let Some(value) = trimmed.strip_prefix("tag:") {
        EntityRef::Tag(value.trim().to_string())
    } else if trimmed == item_id {
        EntityRef::Item(trimmed.to_string())
    } else if trimmed.contains(':') {
        EntityRef::Tag(trimmed.to_string())
    } else {
        EntityRef::External(trimmed.to_string())
    }
}

fn entity_text(entity: &EntityRef) -> String {
    entity.display()
}

fn unique_relation_id(
    store: &TagStore,
    scope: &RelationScope,
    subject: &EntityRef,
    predicate: RelationPredicate,
    object: &EntityRef,
) -> String {
    let base = format!(
        "manual-{}-{}-{}-{}",
        stable_suffix(&scope.key()),
        stable_suffix(&entity_text(subject)),
        stable_suffix(predicate.as_str()),
        stable_suffix(&entity_text(object))
    );
    let mut id = base.clone();
    let mut counter = 2;
    while store.relation(scope, &id).is_some() {
        id = format!("{}-{}", base, counter);
        counter += 1;
    }
    id
}

/// Explicit bridge from accepted core claims back into the sidecar. This is a
/// user action, not an automatic rewrite.
fn sync_accepted_to_sidecar(
    review: &TagReviewStore,
    item_id: &str,
    staged_sidecar: &mut Option<Sidecar>,
    path: Option<&Path>,
) -> Result<usize, String> {
    let sidecar = staged_sidecar.as_mut().ok_or_else(|| "No sidecar loaded".to_string())?;
    let item = review
        .store
        .item(item_id)
        .ok_or_else(|| "No tag assertions for this file".to_string())?;

    // The sidecar is the preservation layer. Rebuild the active projection
    // without dropping tags the user explicitly kept in the file.
    let preserved: Vec<String> = sidecar.preserved_tags.clone();
    let mut tags = item.projection(false);
    for tag in &preserved {
        if !tags.iter().any(|value| value == tag) {
            tags.push(tag.clone());
        }
    }
    tags.dedup();
    sidecar.tags = tags;
    sidecar.preserved_tags = preserved
        .into_iter()
        .filter(|tag| sidecar.tags.contains(tag))
        .collect();
    sidecar.preserved_tags.dedup();

    sidecar.artists = item
        .assertions
        .iter()
        .filter(|assertion| {
            assertion.state == AssertionState::Accepted
                && assertion.tag.starts_with("artist:")
                && !sidecar.is_preserved_tag(&assertion.tag)
        })
        .map(|assertion| assertion.tag.trim_start_matches("artist:").to_string())
        .collect();
    sidecar.relationships = item
        .visible_relations(false)
        .into_iter()
        .map(|relation| Relationship {
            subject: entity_text(&relation.subject),
            predicate: relation.predicate.as_str().to_string(),
            object: entity_text(&relation.object),
        })
        .collect();

    if let Some(path) = path {
        sidecar.save(path).map_err(|error| error.to_string())?;
    }
    Ok(sidecar.tags.len() + sidecar.artists.len() + sidecar.relationships.len())
}

fn draw_relation_row(
    ui: &mut Ui,
    review: &mut TagReviewStore,
    state: &mut ReviewUiState,
    scope: RelationScope,
    relation: &TagRelation,
) {
    ui.push_id(format!("relation:{}:{}", scope.key(), relation.id), |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(state_label(relation.state))
                    .size(10.0)
                    .strong()
                    .color(state_color(relation.state)),
            );
            ui.label(RichText::new(entity_text(&relation.subject)).size(12.0));
            ui.label(RichText::new(relation.predicate.as_str()).size(12.0).monospace());
            ui.label(RichText::new(entity_text(&relation.object)).size(12.0));
            ui.label(RichText::new(format!("{:.0}%", relation.confidence * 100.0)).size(10.0).color(Color32::GRAY));
            if relation.locked {
                ui.label(RichText::new("locked").size(10.0).color(Color32::LIGHT_GRAY));
            }
        });
        ui.label(RichText::new(provenance_line(&relation.provenance)).size(10.0).color(Color32::GRAY));
        if let Some(evidence) = relation.evidence.first() {
            ui.label(RichText::new(evidence).size(10.0).color(Color32::from_gray(170)));
        }
        ui.horizontal(|ui| {
            let change = match relation.state {
                AssertionState::Suggested => {
                    if ui.button("Accept").clicked() {
                        Some(AssertionState::Accepted)
                    } else if ui.button("Reject").clicked() {
                        Some(AssertionState::Rejected)
                    } else {
                        None
                    }
                }
                AssertionState::Accepted => {
                    if ui.button("Reject").clicked() {
                        Some(AssertionState::Rejected)
                    } else {
                        None
                    }
                }
                AssertionState::Rejected | AssertionState::Superseded => {
                    if ui.button("Accept").clicked() {
                        Some(AssertionState::Accepted)
                    } else {
                        None
                    }
                }
            };
            if let Some(next) = change {
                match review.set_relation_state_scoped(scope.clone(), &relation.id, next) {
                    Ok(()) => state.status = format!("relation {} is now {:?}", relation.id, next),
                    Err(error) => state.status = format!("Could not update relation: {}", error),
                }
            }
            let lock_label = if relation.locked { "Unlock" } else { "Lock" };
            if ui.button(lock_label).clicked()
                && !review.toggle_relation_lock_scoped(scope.clone(), &relation.id, !relation.locked)
            {
                state.status = format!("Could not change lock for {}", relation.id);
            }
        });
    });
    ui.separator();
}

/// Draw the review surface inside its host. The outcome tells the app which deferred actions need
/// worker threads and whether accepted claims were synced to the sidecar.
pub fn draw_tag_review(
    ui: &mut Ui,
    review: &mut TagReviewStore,
    item_id: &str,
    staged_sidecar: &mut Option<Sidecar>,
    metadata_path: Option<&Path>,
    state: &mut ReviewUiState,
) -> ReviewOutcome {
    let mut local_outcome = ReviewOutcome::default();

    ui.label(RichText::new("Review").strong());
    ui.label(
        RichText::new(format!("store revision {}", review.store.revision()))
            .size(10.0)
            .color(Color32::GRAY),
    );
    if !state.status.is_empty() {
        ui.label(RichText::new(&state.status).size(11.0).color(Color32::LIGHT_GRAY));
    }
    ui.horizontal_wrapped(|ui| {
        if ui.button("Import sidecar").clicked() {
            let fallback = Sidecar::default();
            let current = staged_sidecar.as_ref().unwrap_or(&fallback);
            let count = review.import_sidecar(item_id, current);
            state.status = format!("Imported {} sidecar claim(s)", count);
        }
        if ui.button("Suggest artist metadata").clicked() {
            let fallback = Sidecar::default();
            let current = staged_sidecar.as_ref().unwrap_or(&fallback);
            let (applied, rejected) = review.suggest_artist_metadata(item_id, current);
            state.status = format!("Added {} suggestion(s), rejected {}", applied, rejected);
        }
        if local_vision::enabled() {
            if ui.button("Suggest local features").clicked() {
                local_outcome.local_vision = Some(LocalVisionJob::features(item_id, item_id));
            }
        } else {
            ui.label(
                RichText::new("Local vision disabled (build with --features local-vision)")
                    .size(10.0)
                    .color(Color32::GRAY),
            );
        }
        if ui.button("Sync accepted → sidecar").clicked() {
            match sync_accepted_to_sidecar(review, item_id, staged_sidecar, metadata_path) {
                Ok(count) => {
                    state.status = format!("Synced {} accepted claim(s) to sidecar", count);
                    local_outcome.synced = true;
                }
                Err(error) => state.status = format!("Sync failed: {}", error),
            }
        }
    });
    ui.separator();

    if local_vision::onnx_enabled() {
        ui.collapsing("Local ONNX model (explicit files only)", |ui| {
            ui.label(
                RichText::new("No downloads. Model guesses stay suggestions until reviewed.")
                    .size(10.0)
                    .color(Color32::GRAY),
            );
            ui.horizontal_wrapped(|ui| {
                ui.label("Runtime");
                ui.add_sized([240.0, 20.0], TextEdit::singleline(&mut state.onnx.runtime_path));
                ui.label("Model");
                ui.add_sized([240.0, 20.0], TextEdit::singleline(&mut state.onnx.model_path));
                ui.label("Labels");
                ui.add_sized([240.0, 20.0], TextEdit::singleline(&mut state.onnx.labels_path));
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Input");
                ui.add_sized([100.0, 20.0], TextEdit::singleline(&mut state.onnx.input_name));
                ui.label("Output");
                ui.add_sized([100.0, 20.0], TextEdit::singleline(&mut state.onnx.output_name));
                ui.label("Version");
                ui.add_sized([120.0, 20.0], TextEdit::singleline(&mut state.onnx.model_version));
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Layout");
                ui.radio_value(&mut state.onnx.layout, OnnxInputLayout::Nchw, "NCHW");
                ui.radio_value(&mut state.onnx.layout, OnnxInputLayout::Nhwc, "NHWC");
                ui.label("Pixels");
                ui.radio_value(&mut state.onnx.normalization, OnnxNormalization::None, "raw");
                ui.radio_value(&mut state.onnx.normalization, OnnxNormalization::ZeroOne, "0–1");
                ui.radio_value(&mut state.onnx.normalization, OnnxNormalization::Clip, "CLIP");
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Output values");
                ui.radio_value(
                    &mut state.onnx.output,
                    OnnxOutputInterpretation::Logits,
                    "logits",
                );
                ui.radio_value(
                    &mut state.onnx.output,
                    OnnxOutputInterpretation::Probabilities,
                    "probabilities",
                );
                ui.add(
                    egui::DragValue::new(&mut state.onnx.input_size)
                        .clamp_range(32..=1024)
                        .prefix("size "),
                );
                ui.add(
                    egui::DragValue::new(&mut state.onnx.top_k)
                        .clamp_range(1..=50)
                        .prefix("top "),
                );
                ui.add(
                    egui::Slider::new(&mut state.onnx.min_confidence, 0.0..=0.99)
                        .text("min")
                        .fixed_decimals(2),
                );
            });
            if ui.button("Suggest ONNX labels").clicked() {
                match state.onnx.validate() {
                    Ok(()) => {
                        local_outcome.local_vision = Some(LocalVisionJob::onnx(
                            item_id,
                            item_id,
                            state.onnx.clone(),
                        ));
                        state.status = "Running local ONNX model...".to_owned();
                    }
                    Err(error) => state.status = format!("ONNX setup error: {error}"),
                }
            }
        });
    } else {
        ui.label(
            RichText::new("ONNX model disabled (build with --features onnx-vision)")
                .size(10.0)
                .color(Color32::GRAY),
        );
    }
    ui.separator();

    draw_source_editor(
        ui,
        state,
        staged_sidecar,
        metadata_path,
        &mut local_outcome,
    );
    ui.separator();

    draw_source_search(
        ui,
        review,
        state,
        item_id,
        staged_sidecar.as_ref().map(|sidecar| sidecar as &Sidecar),
        &mut local_outcome,
    );
    ui.separator();

    let assertion_rows: Vec<AssertionRow> = review
        .store
        .item(item_id)
        .map(|item| {
            item.assertions
                .iter()
                .map(|assertion| AssertionRow {
                    tag: assertion.tag.clone(),
                    state: assertion.state,
                    confidence: assertion.confidence,
                    provenance: assertion.provenance.clone(),
                    evidence: assertion.evidence.clone(),
                    locked: assertion.locked,
                })
                .collect()
        })
        .unwrap_or_default();

    let preserved_rows = preservation_rows(review, item_id, staged_sidecar.as_ref());

    let relation_rows: Vec<TagRelation> = review
        .store
        .item(item_id)
        .map(|item| item.relations.clone())
        .unwrap_or_default();
    let global_relation_rows: Vec<TagRelation> = review.store.global_relations().to_vec();

    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.label(RichText::new("Assertions").strong());
        if assertion_rows.is_empty() {
            ui.label(RichText::new("No assertions. Import the sidecar first.").size(11.0).color(Color32::GRAY));
        }
        for row in &assertion_rows {
            ui.push_id(format!("assertion:{}", row.tag), |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(state_label(row.state))
                            .size(10.0)
                            .strong()
                            .color(state_color(row.state)),
                    );
                    ui.label(RichText::new(&row.tag).size(12.0).monospace());
                    ui.label(RichText::new(format!("{:.0}%", row.confidence * 100.0)).size(10.0).color(Color32::GRAY));
                    if row.locked {
                        ui.label(RichText::new("locked").size(10.0).color(Color32::LIGHT_GRAY));
                    }
                });
                ui.label(RichText::new(provenance_line(&row.provenance)).size(10.0).color(Color32::GRAY));
                if let Some(evidence) = row.evidence.first() {
                    ui.label(RichText::new(evidence).size(10.0).color(Color32::from_gray(170)));
                }
                ui.horizontal(|ui| {
                    let change = match row.state {
                        AssertionState::Suggested => {
                            if ui.button("Accept").clicked() {
                                Some(AssertionState::Accepted)
                            } else if ui.button("Reject").clicked() {
                                Some(AssertionState::Rejected)
                            } else {
                                None
                            }
                        }
                        AssertionState::Accepted => {
                            if ui.button("Reject").clicked() {
                                Some(AssertionState::Rejected)
                            } else {
                                None
                            }
                        }
                        AssertionState::Rejected | AssertionState::Superseded => {
                            if ui.button("Accept").clicked() {
                                Some(AssertionState::Accepted)
                            } else {
                                None
                            }
                        }
                    };
                    if let Some(next) = change {
                        if next == AssertionState::Accepted {
                            if let Some(sidecar) = staged_sidecar.as_mut() {
                                if sidecar.is_preserved_tag(&row.tag) {
                                    sidecar.set_tag_preserved(&row.tag, false);
                                    local_outcome.preservation_changed = true;
                                }
                            }
                        }
                        match review.set_assertion_state(item_id, &row.tag, next) {
                            Ok(()) => state.status = format!("{} is now {:?}", row.tag, next),
                            Err(error) => state.status = format!("Could not update {}: {}", row.tag, error),
                        }
                    }
                    let lock_label = if row.locked { "Unlock" } else { "Lock" };
                    if ui.button(lock_label).clicked()
                        && !review.toggle_assertion_lock(item_id, &row.tag, !row.locked)
                    {
                        state.status = format!("Could not change lock for {}", row.tag);
                    }
                });
            });
            ui.separator();
        }

        ui.add_space(6.0);
        ui.collapsing(
            format!(
                "Version history ({})",
                staged_sidecar.as_ref().map_or(0, |sidecar| sidecar.history.len())
            ),
            |ui| {
                ui.label(
                    RichText::new("Last three metadata generations. Restore never touches cached pixels.")
                        .size(10.0)
                        .color(Color32::GRAY),
                );
                let history = staged_sidecar
                    .as_ref()
                    .map(|sidecar| sidecar.history.clone())
                    .unwrap_or_default();
                if history.is_empty() {
                    ui.label(
                        RichText::new("No saved generations yet.")
                            .size(11.0)
                            .color(Color32::GRAY),
                    );
                }
                for (index, snapshot) in history.iter().enumerate().rev() {
                    ui.push_id(format!("history:{}", index), |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(format!("v{}", snapshot.version))
                                    .size(10.0)
                                    .strong(),
                            );
                            ui.label(
                                RichText::new(snapshot.tags.join(" · "))
                                    .size(10.0)
                                    .color(Color32::GRAY),
                            );
                        });
                        if ui.small_button("Restore this version").clicked() {
                            if let Some(sidecar) = staged_sidecar.as_mut() {
                                if sidecar.restore_snapshot(index) {
                                    local_outcome.preservation_changed = true;
                                    state.status = format!(
                                        "Restored sidecar metadata from v{}",
                                        snapshot.version
                                    );
                                }
                            }
                        }
                    });
                    ui.separator();
                }
            },
        );

        ui.add_space(6.0);
        ui.collapsing(
            format!("Preserved in file ({})", preserved_rows.len()),
            |ui| {
                ui.label(
                    RichText::new("Sidecar-only, rejected, superseded, or explicitly preserved tags stay on disk but do not feed the active gallery index.")
                        .size(10.0)
                        .color(Color32::GRAY),
                );
                if preserved_rows.is_empty() {
                    ui.label(
                        RichText::new("No preserved tags for this file.")
                            .size(11.0)
                            .color(Color32::GRAY),
                    );
                }
                for row in &preserved_rows {
                    ui.push_id(format!("preserved:{}", row.tag), |ui| {
                        ui.horizontal_wrapped(|ui| {
                            let label = if row.preserved {
                                "PRESERVED"
                            } else {
                                row.state.map(state_label).unwrap_or("ONLY IN FILE")
                            };
                            let color = if row.preserved {
                                Color32::from_rgb(180, 160, 240)
                            } else {
                                row.state.map(state_color).unwrap_or(Color32::from_gray(170))
                            };
                            ui.label(RichText::new(label).size(10.0).strong().color(color));
                            ui.label(RichText::new(&row.tag).size(12.0).monospace());
                            ui.label(
                                RichText::new(format!("sidecar v{}", row.sidecar_version))
                                    .size(10.0)
                                    .color(Color32::GRAY),
                            );
                        });
                        if let Some(replacement) = &row.superseded_by {
                            ui.label(
                                RichText::new(format!("superseded by {}", replacement))
                                    .size(10.0)
                                    .color(Color32::GRAY),
                            );
                        }
                        ui.horizontal(|ui| {
                            let button = if row.preserved {
                                "Restore to index"
                            } else {
                                "Keep in file"
                            };
                            if ui.small_button(button).clicked() {
                                if let Some(sidecar) = staged_sidecar.as_mut() {
                                    sidecar.set_tag_preserved(&row.tag, !row.preserved);
                                    local_outcome.preservation_changed = true;
                                    state.status = if row.preserved {
                                        format!("{} restored to the active index", row.tag)
                                    } else {
                                        format!("{} preserved in the sidecar", row.tag)
                                    };
                                }
                            }
                        });
                    });
                    ui.separator();
                }
            },
        );

        ui.add_space(6.0);
        ui.label(RichText::new("Relations for this file").strong());
        if relation_rows.is_empty() {
            ui.label(RichText::new("No relations recorded.").size(11.0).color(Color32::GRAY));
        }
        for relation in &relation_rows {
            draw_relation_row(
                ui,
                review,
                state,
                RelationScope::Item(item_id.to_string()),
                relation,
            );
        }

        ui.collapsing("Add relation for this file", |ui| {
            ui.horizontal(|ui| {
                ui.label("Subject");
                ui.add_sized([120.0, 20.0], TextEdit::singleline(&mut state.subject).hint_text("item:this"));
            });
            ui.horizontal(|ui| {
                ui.label("Predicate");
                ui.add_sized([120.0, 20.0], TextEdit::singleline(&mut state.predicate).hint_text("created_by"));
            });
            ui.horizontal(|ui| {
                ui.label("Object");
                ui.add_sized([120.0, 20.0], TextEdit::singleline(&mut state.object).hint_text("tag:artist:kitty"));
            });
            ui.label(RichText::new("item:file  tag:id  plain external name").size(10.0).color(Color32::GRAY));
            if ui.button("Add accepted relation").clicked() {
                let subject = entity_from_text(item_id, &state.subject);
                let predicate = RelationPredicate::parse(&state.predicate);
                let object = entity_from_text(item_id, &state.object);
                if entity_text(&subject).is_empty() || entity_text(&object).is_empty() {
                    state.status = "Subject and object cannot be empty".to_string();
                } else {
                    let scope = RelationScope::Item(item_id.to_string());
                    let id = unique_relation_id(review.store_ref(), &scope, &subject, predicate, &object);
                    match review.add_relation_scoped(scope, id.clone(), subject, predicate, object) {
                        Ok(()) => {
                            state.subject.clear();
                            state.predicate.clear();
                            state.object.clear();
                            state.status = format!("Added relation {}", id);
                        }
                        Err(error) => state.status = format!("Could not add relation: {}", error),
                    }
                }
            }
        });

        ui.add_space(10.0);
        ui.label(RichText::new("Global relations").strong());
        ui.label(
            RichText::new("Rules apply to every image carrying one of their tag endpoints.")
                .size(10.0)
                .color(Color32::GRAY),
        );
        if global_relation_rows.is_empty() {
            ui.label(RichText::new("No global relations recorded.").size(11.0).color(Color32::GRAY));
        }
        for relation in &global_relation_rows {
            draw_relation_row(ui, review, state, RelationScope::Global, relation);
        }

        ui.collapsing("Add global relation", |ui| {
            ui.horizontal(|ui| {
                ui.label("Subject");
                ui.add_sized([120.0, 20.0], TextEdit::singleline(&mut state.global_subject).hint_text("tag:theme:ownership"));
            });
            ui.horizontal(|ui| {
                ui.label("Predicate");
                ui.add_sized([120.0, 20.0], TextEdit::singleline(&mut state.global_predicate).hint_text("realizes"));
            });
            ui.horizontal(|ui| {
                ui.label("Object");
                ui.add_sized([120.0, 20.0], TextEdit::singleline(&mut state.global_object).hint_text("tag:act:possession"));
            });
            ui.label(RichText::new("Use tag:id endpoints for rules that apply across the library.").size(10.0).color(Color32::GRAY));
            if ui.button("Add accepted global relation").clicked() {
                let subject = entity_from_text("", &state.global_subject);
                let predicate = RelationPredicate::parse(&state.global_predicate);
                let object = entity_from_text("", &state.global_object);
                if entity_text(&subject).is_empty() || entity_text(&object).is_empty() {
                    state.status = "Subject and object cannot be empty".to_string();
                } else {
                    let id = unique_relation_id(review.store_ref(), &RelationScope::Global, &subject, predicate, &object);
                    match review.add_relation_scoped(
                        RelationScope::Global,
                        id.clone(),
                        subject,
                        predicate,
                        object,
                    ) {
                        Ok(()) => {
                            state.global_subject.clear();
                            state.global_predicate.clear();
                            state.global_object.clear();
                            state.status = format!("Added global relation {}", id);
                        }
                        Err(error) => state.status = format!("Could not add global relation: {}", error),
                    }
                }
            }
        });
    });

    local_outcome
}

impl TagReviewStore {
    fn store_ref(&self) -> &TagStore {
        &self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symtropy_tags::{ProvenanceKind, TagDefinition};

    #[test]
    fn entity_text_inputs_are_classified() {
        assert!(matches!(entity_from_text("a.webp", "item:b.webp"), EntityRef::Item(id) if id == "b.webp"));
        assert!(matches!(entity_from_text("a.webp", "tag:artist:kitty"), EntityRef::Tag(id) if id == "artist:kitty"));
        assert!(matches!(entity_from_text("a.webp", "kitty"), EntityRef::External(id) if id == "kitty"));
        assert!(matches!(entity_from_text("a.webp", "a.webp"), EntityRef::Item(id) if id == "a.webp"));
    }

    #[test]
    fn import_never_overwrites_an_existing_human_claim() {
        let mut review = TagReviewStore::default();
        review
            .store
            .registry_mut()
            .insert(TagDefinition::new("artist:kitty", "Kitty", symtropy_tags::TagKind::Creator))
            .unwrap();
        review.store.item_mut("a.webp").upsert_assertion(
            TagAssertion::accepted("artist:kitty", Provenance::human()).locked(true),
        );

        let mut sidecar = Sidecar::default();
        sidecar.tags.push("fox".to_string());
        sidecar.artists.push("kitty".to_string());

        assert_eq!(review.import_sidecar("a.webp", &sidecar), 1);
        let item = review.store.item("a.webp").unwrap();
        let artist = item.assertion("artist:kitty").unwrap();
        assert_eq!(artist.provenance.kind, ProvenanceKind::Human);
        assert!(artist.locked);
        assert_eq!(item.projection(false), vec!["artist:kitty".to_string(), "fox".to_string()]);
    }

    #[test]
    fn sidecar_sources_are_captured_as_suggested_evidence() {
        let mut review = TagReviewStore::default();
        let sidecar = Sidecar {
            sources: vec![crate::gallery::sidecar::Source {
                url: "https://e621.net/posts/123".to_string(),
                site: "e621".to_string(),
                title: "Example".to_string(),
                ..crate::gallery::sidecar::Source::default()
            }],
            ..Sidecar::default()
        };

        assert_eq!(review.capture_sidecar_sources("a.webp", &sidecar), 1);
        assert_eq!(review.capture_sidecar_sources("a.webp", &sidecar), 0);

        let item = review.store.item("a.webp").expect("captured item");
        assert!(item.assertions.is_empty());
        let relation = item.relations.first().expect("source evidence relation");
        assert_eq!(relation.state, AssertionState::Suggested);
        assert_eq!(relation.predicate, RelationPredicate::Evidences);
        assert_eq!(relation.provenance.kind, ProvenanceKind::Source);
        assert!(matches!(&relation.subject, EntityRef::External(url) if url == "https://e621.net/posts/123"));
    }

    #[test]
    fn source_capture_does_not_touch_accepted_or_locked_relations() {
        let mut review = TagReviewStore::default();
        let accepted = "https://e621.net/posts/1";
        let locked = "https://e621.net/posts/2";
        let sidecar = Sidecar {
            sources: vec![
                crate::gallery::sidecar::Source { url: accepted.to_string(), ..Default::default() },
                crate::gallery::sidecar::Source { url: locked.to_string(), ..Default::default() },
            ],
            ..Sidecar::default()
        };
        let accepted_id = format!("source-{}-{}", stable_suffix("a.webp"), stable_suffix(accepted));
        let locked_id = format!("source-{}-{}", stable_suffix("a.webp"), stable_suffix(locked));
        let scope = RelationScope::Item("a.webp".to_string());
        review
            .add_relation_scoped(
                scope.clone(),
                accepted_id.clone(),
                EntityRef::External("human-choice".to_string()),
                RelationPredicate::Evidences,
                EntityRef::Item("a.webp".to_string()),
            )
            .unwrap();
        review
            .add_relation_scoped(
                scope.clone(),
                locked_id.clone(),
                EntityRef::External("locked-choice".to_string()),
                RelationPredicate::Evidences,
                EntityRef::Item("a.webp".to_string()),
            )
            .unwrap();
        review.toggle_relation_lock_scoped(scope, &locked_id, true);

        assert_eq!(review.capture_sidecar_sources("a.webp", &sidecar), 0);

        let item = review.store.item("a.webp").unwrap();
        let accepted_relation = item.relation(&accepted_id).unwrap();
        assert_eq!(accepted_relation.state, AssertionState::Accepted);
        assert!(matches!(&accepted_relation.subject, EntityRef::External(name) if name == "human-choice"));
        let locked_relation = item.relation(&locked_id).unwrap();
        assert!(locked_relation.locked);
        assert!(matches!(&locked_relation.subject, EntityRef::External(name) if name == "locked-choice"));
    }

    #[test]
    fn local_suggestions_are_staged_without_overwriting_accepted_tags() {
        let mut review = TagReviewStore::default();
        review.store.item_mut("a.webp").upsert_assertion(
            TagAssertion::accepted(
                "feature:color:dominant_orange",
                Provenance::human(),
            )
            .locked(true),
        );
        let suggestions = vec![
            LocalSuggestion {
                tag: "feature:color:dominant_orange".to_string(),
                confidence: 0.62,
                reason: "dominant hue".to_string(),
                model_version: "local-features-v1".to_string(),
            },
            LocalSuggestion {
                tag: "feature:tone:light".to_string(),
                confidence: 0.45,
                reason: "bright image".to_string(),
                model_version: "local-features-v1".to_string(),
            },
        ];

        let (applied, rejected) = review.stage_local_suggestions("a.webp", &suggestions);
        assert_eq!(applied, 1);
        assert_eq!(rejected, 1);

        let item = review.store.item("a.webp").unwrap();
        let accepted = item.assertion("feature:color:dominant_orange").unwrap();
        assert_eq!(accepted.state, AssertionState::Accepted);
        assert!(accepted.locked);
        let tone = item.assertion("feature:tone:light").unwrap();
        assert_eq!(tone.state, AssertionState::Suggested);
        assert_eq!(tone.provenance.kind, ProvenanceKind::Ai);
        assert_eq!(tone.provenance.model_version, "local-features-v1");
    }

    #[test]
    fn global_relations_can_be_added_reviewed_and_locked() {
        let mut review = TagReviewStore::default();
        let scope = RelationScope::Global;
        let subject = EntityRef::Tag("theme:ownership".to_string());
        let object = EntityRef::Tag("act:possession".to_string());
        let predicate = RelationPredicate::Realizes;
        let id = unique_relation_id(review.store_ref(), &scope, &subject, predicate, &object);
        assert!(id.starts_with("manual-global-"));

        review
            .add_relation_scoped(
                scope.clone(),
                id.clone(),
                subject,
                predicate,
                object,
            )
            .unwrap();
        assert_eq!(
            review.store.relation(&scope, &id).unwrap().state,
            AssertionState::Accepted
        );

        review
            .set_relation_state_scoped(scope.clone(), &id, AssertionState::Suggested)
            .unwrap();
        assert_eq!(
            review.store.relation(&scope, &id).unwrap().state,
            AssertionState::Suggested
        );

        assert!(review.toggle_relation_lock_scoped(scope.clone(), &id, true));
        assert!(review.store.relation(&scope, &id).unwrap().locked);
        assert!(review.store.to_json5_pretty().unwrap().contains(&id));
    }

    #[test]
    fn sync_writes_accepted_claims_back_to_the_sidecar() {
        let mut review = TagReviewStore::default();
        review.store.item_mut("a.webp").upsert_assertion(
            TagAssertion::accepted("artist:kitty", Provenance::human()),
        );
        review.store.item_mut("a.webp").upsert_assertion(
            TagAssertion::suggested("idea:temp", Provenance::ai("test", "v1"), 0.7),
        );

        let mut staged = Some(Sidecar::default());
        let count = sync_accepted_to_sidecar(&review, "a.webp", &mut staged, None).unwrap();
        assert_eq!(count, 2);
        let sidecar = staged.unwrap();
        assert_eq!(sidecar.tags, vec!["artist:kitty".to_string()]);
        assert_eq!(sidecar.artists, vec!["kitty".to_string()]);
    }

    #[test]
    fn sync_keeps_preserved_tags_in_the_sidecar() {
        let mut review = TagReviewStore::default();
        review.store.item_mut("a.webp").upsert_assertion(
            TagAssertion::accepted("artist:kitty", Provenance::human()),
        );
        review.store.item_mut("a.webp").upsert_assertion(
            TagAssertion::rejected("obsolete", Provenance::human()),
        );

        let mut sidecar = Sidecar::default();
        sidecar.tags = vec!["artist:kitty".to_string(), "obsolete".to_string()];
        sidecar.set_tag_preserved("obsolete", true);
        let mut staged = Some(sidecar);

        let _ = sync_accepted_to_sidecar(&review, "a.webp", &mut staged, None).unwrap();
        let sidecar = staged.unwrap();
        assert!(sidecar.tags.contains(&"artist:kitty".to_string()));
        assert!(sidecar.tags.contains(&"obsolete".to_string()));
        assert_eq!(sidecar.preserved_tags, vec!["obsolete".to_string()]);
        assert_eq!(sidecar.artists, vec!["kitty".to_string()]);

        let rows = preservation_rows(&review, "a.webp", Some(&sidecar));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tag, "obsolete");
        assert!(rows[0].preserved);
    }
}
