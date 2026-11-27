#![forbid(unsafe_code)]
//! Local, proposal-only bridge between external agents and Symtropy's tag system.
//!
//! This crate is deliberately small. It does not talk to the network and it
//! never gives an agent raw write access to the tag store. Every mutating
//! request becomes a [`Proposal`], is checked by [`ApplyPolicy`], and is saved
//! atomically only after a successful apply.
//!
//! The CLI in `src/main.rs` exposes the same API over JSON5 on stdin, so any
//! agent that can run a process can use it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use symtropy_tags::{
    artist_definition, artist_tag_id, stable_suffix, ApplyPolicy, AssertionState, Change,
    EntityRef, Proposal, ProposalPreview, Provenance, RejectedChange, RelationPredicate,
    RelationScope, TagAssertion, TagRelation, TagStatus, TagStore,
};

pub const DEFAULT_STORE_FILE: &str = "symtropy_tag_store.json5";

fn default_tool() -> String {
    "agent".to_string()
}

fn default_model_version() -> String {
    "unspecified".to_string()
}

/// Resolves the same config location as the UI's tag review store.
pub fn default_store_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(DEFAULT_STORE_FILE)
}

/// Only safe, review-preserving policies are available to agents. The two
/// dangerous `allow_machine_overwrite_*` flags are never exposed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPolicy {
    #[default]
    Conservative,
    Review,
}

impl AgentPolicy {
    fn apply_policy(self) -> ApplyPolicy {
        match self {
            Self::Conservative => ApplyPolicy::conservative(),
            Self::Review => ApplyPolicy {
                strict_revision: true,
                min_confidence: 0.0,
                allow_machine_overwrite_accepted: false,
                allow_machine_overwrite_locked: false,
            },
        }
    }
}

/// One reviewable machine guess. A confidence of `1.0` is rejected because
/// accepted claims must come from an explicit human policy, not an agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentSuggestion {
    pub tag: String,
    pub confidence: f32,
    #[serde(default)]
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtistSidecarInput {
    pub item_id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub artists: Vec<String>,
    /// Child tags whose parent is `artist` or `creator` in the sidecar's
    /// `tag_parents` map. These are artist-looking flat tags, not proof.
    #[serde(default)]
    pub artist_hints: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreservationSidecarInput {
    pub item_id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub preserved_tags: Vec<String>,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub artist_hints: Vec<String>,
    #[serde(default)]
    pub version: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreservationSource {
    Tag,
    Artist,
    ArtistHint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreservationStatus {
    OnlyInSidecar,
    Preserved,
    Accepted,
    Suggested,
    Rejected,
    Superseded,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreservationEntry {
    pub item_id: String,
    pub tag: String,
    pub source: PreservationSource,
    pub sidecar_version: u32,
    pub status: PreservationStatus,
    #[serde(default)]
    pub store_confidence: Option<f32>,
    #[serde(default)]
    pub store_provenance: Option<String>,
    #[serde(default)]
    pub superseded_by: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentRequest {
    Status,
    PreviewProposal {
        proposal: Proposal,
        #[serde(default)]
        policy: AgentPolicy,
    },
    ApplyProposal {
        proposal: Proposal,
        #[serde(default)]
        policy: AgentPolicy,
    },
    /// Return a proposal only. Nothing is written.
    ProposeArtists {
        item_id: String,
        tags: Vec<String>,
        #[serde(default)]
        artist_hints: Vec<String>,
    },
    /// Stage explicit `sidecar.artists` as creator metadata. This is an Import
    /// claim because the structured sidecar field is user-owned metadata.
    StageArtistMetadata {
        item_id: String,
        artists: Vec<String>,
    },
    /// Return a proposal for a batch of local sidecars. Nothing is written.
    ProposeArtistSidecars {
        items: Vec<ArtistSidecarInput>,
    },
    /// Apply a batch proposal generated from local sidecars. Explicit
    /// `artists` fields are Import claims; flat-tag guesses stay Suggested.
    StageArtistSidecars {
        items: Vec<ArtistSidecarInput>,
    },
    /// Report sidecar metadata that is not active in the main tag index.
    /// This never writes anything and never edits sidecars.
    BuildPreservationReport {
        items: Vec<PreservationSidecarInput>,
        #[serde(default)]
        include_accepted: bool,
    },
    /// Stage AI/agent output as `Suggested` assertions. Always reviewable.
    StageSuggestions {
        item_id: String,
        suggestions: Vec<AgentSuggestion>,
        #[serde(default = "default_tool")]
        tool: String,
        #[serde(default = "default_model_version")]
        model_version: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentResponse {
    Status {
        revision: u64,
        store_path: Option<String>,
    },
    Proposal {
        proposal: Proposal,
        preview: ProposalPreview,
    },
    Preview {
        revision: u64,
        preview: ProposalPreview,
    },
    PreservationReport {
        revision: u64,
        entries: Vec<PreservationEntry>,
    },
    Applied {
        old_revision: u64,
        new_revision: u64,
        applied: Vec<Change>,
        rejected: Vec<RejectedChange>,
    },
}

fn clean_artist_id(raw: &str) -> Option<String> {
    let id = artist_tag_id(raw);
    let bare = id.strip_prefix("artist:").unwrap_or_default();
    if bare.trim().is_empty() {
        None
    } else {
        Some(id)
    }
}

/// Build a proposal that imports explicit artist fields from a sidecar.
///
/// Unlike flat-tag guessing, these become accepted `Import` claims with a
/// typed `created_by` edge. Existing accepted or locked claims still block the
/// apply through the normal policy.
pub fn propose_artist_import(
    item_id: &str,
    artists: &[String],
    based_on_revision: u64,
) -> Proposal {
    let provenance = Provenance::import("agent:sidecar-artists");
    let mut seen = BTreeSet::new();
    let mut operations = Vec::new();

    for raw in artists {
        let Some(id) = clean_artist_id(raw) else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }

        let mut definition = artist_definition(raw);
        definition.status = TagStatus::Active;
        definition.definition = "Artist metadata imported from a local sidecar.".to_string();
        definition.provenance = provenance.clone();

        operations.push(symtropy_tags::Operation::DefineTag { definition });
        operations.push(symtropy_tags::Operation::AddAssertion {
            item: item_id.to_string(),
            assertion: TagAssertion::accepted(&id, provenance.clone())
                .with_evidence("imported from sidecar.artists by symtropy-agent"),
        });

        let relation_id = format!(
            "{}-created-by-{}",
            stable_suffix(item_id),
            stable_suffix(&id)
        );
        operations.push(symtropy_tags::Operation::AddRelation {
            scope: RelationScope::Item(item_id.to_string()),
            relation: TagRelation::new(
                relation_id,
                EntityRef::Item(item_id.to_string()),
                RelationPredicate::CreatedBy,
                EntityRef::Tag(id),
                AssertionState::Accepted,
                provenance.clone(),
                1.0,
            )
            .with_evidence("imported from sidecar.artists by symtropy-agent"),
        });
    }

    Proposal {
        id: format!("agent-artist-import-{}", stable_suffix(item_id)),
        based_on_revision,
        rationale: "Import explicit sidecar artist metadata as creator assertions and relations."
            .to_string(),
        provenance,
        operations,
    }
}

fn push_operation(
    operation: symtropy_tags::Operation,
    defined: &mut BTreeSet<String>,
    assertions: &mut BTreeSet<(String, String)>,
    relations: &mut BTreeSet<(String, String)>,
    operations: &mut Vec<symtropy_tags::Operation>,
) -> bool {
    match &operation {
        symtropy_tags::Operation::DefineTag { definition } => {
            if !defined.insert(definition.id.clone()) {
                return false;
            }
        }
        symtropy_tags::Operation::AddAssertion { item, assertion } => {
            if !assertions.insert((item.clone(), assertion.tag.clone())) {
                return false;
            }
        }
        symtropy_tags::Operation::AddRelation { scope, relation } => {
            if !relations.insert((scope.key(), relation.id.clone())) {
                return false;
            }
        }
        _ => {}
    }
    operations.push(operation);
    true
}

/// Build one proposal for a batch of local sidecars.
///
/// Explicit `sidecar.artists` are trusted `Import` metadata. Flat tags that
/// look like artist names remain reviewable `Suggested` claims. Both share an
/// Import proposal provenance so the batch can apply without being classified
/// as an untrusted machine overwrite attempt.
pub fn propose_artist_sidecars(
    items: &[ArtistSidecarInput],
    based_on_revision: u64,
) -> Proposal {
    let mut operations = Vec::new();
    let mut defined = BTreeSet::new();
    let mut assertions = BTreeSet::new();
    let mut relations = BTreeSet::new();
    let mut affected = 0usize;

    for entry in items {
        let before = operations.len();
        let explicit_ids: BTreeSet<String> = entry
            .artists
            .iter()
            .filter_map(|artist| clean_artist_id(artist))
            .collect();

        if !entry.artists.is_empty() {
            let proposal = propose_artist_import(&entry.item_id, &entry.artists, based_on_revision);
            for operation in proposal.operations {
                push_operation(
                    operation,
                    &mut defined,
                    &mut assertions,
                    &mut relations,
                    &mut operations,
                );
            }
        }

        let flat_tags: Vec<String> = entry
            .tags
            .iter()
            .filter(|tag| {
                clean_artist_id(tag)
                    .map(|id| !explicit_ids.contains(&id))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if !flat_tags.is_empty() {
            let mut hints = entry.artist_hints.clone();
            hints.extend(entry.artists.iter().cloned());
            let proposal = symtropy_tags::propose_artist_conversion(
                &entry.item_id,
                &flat_tags,
                &hints,
                based_on_revision,
            );
            for operation in proposal.operations {
                push_operation(
                    operation,
                    &mut defined,
                    &mut assertions,
                    &mut relations,
                    &mut operations,
                );
            }
        }

        if operations.len() > before {
            affected += 1;
        }
    }

    Proposal {
        id: format!(
            "agent-sidecar-artists-{}",
            stable_suffix(&format!("r{}-{}", based_on_revision, items.len()))
        ),
        based_on_revision,
        rationale: format!(
            "Convert artist metadata from {} local sidecars into creator tags and relations.",
            affected
        ),
        provenance: Provenance::import("symtropy-agent:sidecar-artists"),
        operations,
    }
}

fn push_preservation_entry(
    store: &TagStore,
    entries: &mut Vec<PreservationEntry>,
    seen: &mut BTreeSet<(String, String)>,
    item_id: &str,
    tag: String,
    source: PreservationSource,
    sidecar_version: u32,
    include_accepted: bool,
    forced_status: Option<PreservationStatus>,
) {
    if tag.trim().is_empty() || !seen.insert((item_id.to_string(), tag.clone())) {
        return;
    }

    if let Some(status) = forced_status {
        let assertion = store.item(item_id).and_then(|item| item.assertion(&tag));
        entries.push(PreservationEntry {
            item_id: item_id.to_string(),
            tag,
            source,
            sidecar_version,
            status,
            store_confidence: assertion.map(|assertion| assertion.confidence),
            store_provenance: assertion.map(|assertion| {
                format!("{:?}:{}", assertion.provenance.kind, assertion.provenance.tool)
            }),
            superseded_by: assertion.and_then(|assertion| assertion.superseded_by.clone()),
        });
        return;
    }

    let assertion = store.item(item_id).and_then(|item| item.assertion(&tag));
    let (status, confidence, provenance, superseded_by) = match assertion {
        None => (PreservationStatus::OnlyInSidecar, None, None, None),
        Some(assertion) => {
            let status = match assertion.state {
                AssertionState::Accepted => PreservationStatus::Accepted,
                AssertionState::Suggested => PreservationStatus::Suggested,
                AssertionState::Rejected => PreservationStatus::Rejected,
                AssertionState::Superseded => PreservationStatus::Superseded,
            };
            if status == PreservationStatus::Accepted && !include_accepted {
                return;
            }
            let provenance = format!(
                "{:?}:{}",
                assertion.provenance.kind, assertion.provenance.tool
            );
            (
                status,
                Some(assertion.confidence),
                Some(provenance),
                assertion.superseded_by.clone(),
            )
        }
    };

    entries.push(PreservationEntry {
        item_id: item_id.to_string(),
        tag,
        source,
        sidecar_version,
        status,
        store_confidence: confidence,
        store_provenance: provenance,
        superseded_by,
    });
}

/// Report sidecar metadata that is missing, hidden, or only weakly present in
/// the tag store. This is read-only: sidecars are the preservation layer and
/// are never edited by the bridge.
pub fn build_preservation_report(
    store: &TagStore,
    items: &[PreservationSidecarInput],
    include_accepted: bool,
) -> Vec<PreservationEntry> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();

    for entry in items {
        for tag in &entry.preserved_tags {
            push_preservation_entry(
                store,
                &mut entries,
                &mut seen,
                &entry.item_id,
                tag.clone(),
                PreservationSource::Tag,
                entry.version,
                include_accepted,
                Some(PreservationStatus::Preserved),
            );
        }
        for tag in &entry.tags {
            push_preservation_entry(
                store,
                &mut entries,
                &mut seen,
                &entry.item_id,
                tag.clone(),
                PreservationSource::Tag,
                entry.version,
                include_accepted,
                None,
            );
        }
        for raw in &entry.artists {
            if let Some(id) = clean_artist_id(raw) {
                push_preservation_entry(
                    store,
                    &mut entries,
                    &mut seen,
                    &entry.item_id,
                    id,
                    PreservationSource::Artist,
                    entry.version,
                    include_accepted,
                    None,
                );
            }
        }
        for raw in &entry.artist_hints {
            if let Some(id) = clean_artist_id(raw) {
                push_preservation_entry(
                    store,
                    &mut entries,
                    &mut seen,
                    &entry.item_id,
                    id,
                    PreservationSource::ArtistHint,
                    entry.version,
                    include_accepted,
                    None,
                );
            }
        }
    }

    entries.sort_by(|left, right| {
        left.item_id
            .cmp(&right.item_id)
            .then(left.tag.cmp(&right.tag))
            .then(
                format!("{:?}", left.source).cmp(&format!("{:?}", right.source)),
            )
    });
    entries
}

/// Build a proposal from arbitrary agent suggestions. Every assertion is
/// `Suggested`, machine-provenanced, and capped below full confidence.
pub fn propose_staged_suggestions(
    item_id: &str,
    suggestions: &[AgentSuggestion],
    tool: &str,
    model_version: &str,
    based_on_revision: u64,
) -> Proposal {
    let provenance = Provenance::ai(tool, model_version);
    let mut seen = BTreeSet::new();
    let operations = suggestions
        .iter()
        .filter(|suggestion| {
            let tag = suggestion.tag.trim();
            !tag.is_empty()
                && suggestion.confidence.is_finite()
                && suggestion.confidence >= 0.0
                && suggestion.confidence < 1.0
                && seen.insert(tag.to_string())
        })
        .map(|suggestion| symtropy_tags::Operation::AddAssertion {
            item: item_id.to_string(),
            assertion: TagAssertion::suggested(
                suggestion.tag.trim(),
                provenance.clone(),
                suggestion.confidence,
            )
            .with_evidence(&suggestion.reason),
        })
        .collect();

    Proposal {
        id: format!("agent-suggestions-{}", stable_suffix(item_id)),
        based_on_revision,
        rationale: "Reviewable suggestions staged by an external agent.".to_string(),
        provenance,
        operations,
    }
}

/// External proposals are untrusted. If an agent claims Human or Import
/// provenance, downgrade the whole proposal to AI so it cannot create accepted
/// claims or overwrite human facts. Proposals generated inside the bridge
/// (`ProposeArtists`, sidecar helpers) keep their real provenance.
fn sanitize_external_proposal(mut proposal: Proposal) -> Proposal {
    if !proposal.provenance.is_machine() {
        proposal.provenance = Provenance::ai("external-agent", "untrusted");
    }
    proposal
}

/// A proposal-only handle on the shared tag store file.
pub struct AgentBridge {
    store: TagStore,
    path: Option<PathBuf>,
}

impl AgentBridge {
    pub fn new(store: TagStore, path: Option<PathBuf>) -> Self {
        Self { store, path }
    }

    pub fn load(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let store = if path.exists() {
            let text = fs::read_to_string(&path)
                .map_err(|error| format!("could not read {}: {}", path.display(), error))?;
            TagStore::from_json5(&text)
                .map_err(|error| format!("could not parse {}: {}", path.display(), error))?
        } else {
            TagStore::default()
        };
        Ok(Self::new(store, Some(path)))
    }

    pub fn load_default() -> Result<Self, String> {
        Self::load(default_store_path())
    }

    pub fn store(&self) -> &TagStore {
        &self.store
    }

    #[cfg(test)]
    pub fn store_mut_for_tests(&mut self) -> &mut TagStore {
        &mut self.store
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Atomically replace the store file. Refuses to write if another process
    /// advanced the revision while this bridge was open.
    pub fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };

        if path.exists() {
            let current = fs::read_to_string(path)
                .map_err(|error| format!("could not read {}: {}", path.display(), error))?;
            let disk = TagStore::from_json5(&current)
                .map_err(|error| format!("could not parse {}: {}", path.display(), error))?;
            if disk.revision() != self.store.revision() {
                return Err(format!(
                    "tag store changed on disk while the bridge was open ({} -> {}); retry",
                    disk.revision(),
                    self.store.revision()
                ));
            }
        }

        let text = self.store.to_json5_pretty()?;
        if fs::read_to_string(path).ok().as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {}", parent.display(), error))?;
        }
        let tmp = path.with_extension("json5.tmp");
        fs::write(&tmp, &text)
            .map_err(|error| format!("could not write {}: {}", tmp.display(), error))?;
        fs::rename(&tmp, path)
            .map_err(|error| format!("could not replace {}: {}", path.display(), error))?;
        Ok(())
    }

    pub fn handle(&mut self, request: AgentRequest) -> Result<AgentResponse, String> {
        match request {
            AgentRequest::Status => Ok(AgentResponse::Status {
                revision: self.store.revision(),
                store_path: self.path.as_ref().map(|path| path.display().to_string()),
            }),
            AgentRequest::PreviewProposal { proposal, policy } => {
                let proposal = sanitize_external_proposal(proposal);
                let preview = self.store.preview(&proposal, &policy.apply_policy());
                Ok(AgentResponse::Preview {
                    revision: self.store.revision(),
                    preview,
                })
            }
            AgentRequest::ApplyProposal { proposal, policy } => {
                let proposal = sanitize_external_proposal(proposal);
                let old_revision = self.store.revision();
                let report = self
                    .store
                    .apply_proposal(&proposal, &policy.apply_policy())?;
                if !report.applied.is_empty() {
                    self.save()?;
                }
                Ok(AgentResponse::Applied {
                    old_revision,
                    new_revision: self.store.revision(),
                    applied: report.applied,
                    rejected: report.rejected,
                })
            }
            AgentRequest::ProposeArtists {
                item_id,
                tags,
                artist_hints,
            } => {
                let proposal = symtropy_tags::propose_artist_conversion(
                    &item_id,
                    &tags,
                    &artist_hints,
                    self.store.revision(),
                );
                let preview = self
                    .store
                    .preview(&proposal, &AgentPolicy::Conservative.apply_policy());
                Ok(AgentResponse::Proposal { proposal, preview })
            }
            AgentRequest::StageArtistMetadata { item_id, artists } => {
                let proposal = propose_artist_import(&item_id, &artists, self.store.revision());
                let report = self
                    .store
                    .apply_proposal(&proposal, &AgentPolicy::Review.apply_policy())?;
                if !report.applied.is_empty() {
                    self.save()?;
                }
                Ok(AgentResponse::Applied {
                    old_revision: report.old_revision,
                    new_revision: report.new_revision,
                    applied: report.applied,
                    rejected: report.rejected,
                })
            }
            AgentRequest::ProposeArtistSidecars { items } => {
                let proposal = propose_artist_sidecars(&items, self.store.revision());
                let preview = self
                    .store
                    .preview(&proposal, &AgentPolicy::Conservative.apply_policy());
                Ok(AgentResponse::Proposal { proposal, preview })
            }
            AgentRequest::StageArtistSidecars { items } => {
                let proposal = propose_artist_sidecars(&items, self.store.revision());
                let report = self
                    .store
                    .apply_proposal(&proposal, &AgentPolicy::Conservative.apply_policy())?;
                if !report.applied.is_empty() {
                    self.save()?;
                }
                Ok(AgentResponse::Applied {
                    old_revision: report.old_revision,
                    new_revision: report.new_revision,
                    applied: report.applied,
                    rejected: report.rejected,
                })
            }
            AgentRequest::BuildPreservationReport {
                items,
                include_accepted,
            } => Ok(AgentResponse::PreservationReport {
                revision: self.store.revision(),
                entries: build_preservation_report(&self.store, &items, include_accepted),
            }),
            AgentRequest::StageSuggestions {
                item_id,
                suggestions,
                tool,
                model_version,
            } => {
                let proposal = propose_staged_suggestions(
                    &item_id,
                    &suggestions,
                    &tool,
                    &model_version,
                    self.store.revision(),
                );
                let report = self
                    .store
                    .apply_proposal(&proposal, &AgentPolicy::Review.apply_policy())?;
                if !report.applied.is_empty() {
                    self.save()?;
                }
                Ok(AgentResponse::Applied {
                    old_revision: report.old_revision,
                    new_revision: report.new_revision,
                    applied: report.applied,
                    rejected: report.rejected,
                })
            }
        }
    }

    /// Parse one JSON5 request and return one pretty JSON response.
    pub fn handle_json(&mut self, input: &str) -> Result<String, String> {
        let request: AgentRequest = json5::from_str(input)
            .map_err(|error| format!("invalid agent request: {}", error))?;
        let response = self.handle(request)?;
        serde_json::to_string_pretty(&response)
            .map_err(|error| format!("could not serialize agent response: {}", error))
    }
}

pub fn error_json(message: impl AsRef<str>) -> String {
    let value = serde_json::json!({
        "kind": "error",
        "error": message.as_ref(),
    });
    serde_json::to_string_pretty(&value)
        .unwrap_or_else(|_| "{\"kind\":\"error\",\"error\":\"serialization failed\"}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use symtropy_tags::TagKind;

    fn temp_path(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "symtropy-agent-{}-{}-{}",
            label,
            std::process::id(),
            n
        ))
    }

    fn remove_test_file(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(path.with_extension("json5.tmp"));
    }

    #[test]
    fn json5_request_round_trip_defaults_to_conservative() {
        let request: AgentRequest = json5::from_str(
            r#"{
                kind: 'apply_proposal',
                proposal: { id: 'test', based_on_revision: 0 },
            }"#,
        )
        .expect("request");
        match request {
            AgentRequest::ApplyProposal { policy, .. } => {
                assert_eq!(policy, AgentPolicy::Conservative);
            }
            other => panic!("unexpected request: {:?}", other),
        }
    }

    #[test]
    fn agent_cannot_create_accepted_machine_claims() {
        let mut bridge = AgentBridge::new(TagStore::default(), None);
        let proposal = Proposal {
            id: "agent-accepted".to_string(),
            based_on_revision: bridge.store().revision(),
            rationale: "test".to_string(),
            provenance: Provenance::ai("agent", "test"),
            operations: vec![symtropy_tags::Operation::AddAssertion {
                item: "a.webp".to_string(),
                assertion: TagAssertion::accepted("artist:kitty", Provenance::ai("agent", "test")),
            }],
        };
        let response = bridge
            .handle(AgentRequest::ApplyProposal {
                proposal,
                policy: AgentPolicy::Conservative,
            })
            .expect("response");
        match response {
            AgentResponse::Applied { applied, rejected, .. } => {
                assert!(applied.is_empty());
                assert_eq!(rejected.len(), 1);
            }
            other => panic!("unexpected response: {:?}", other),
        }
        assert_eq!(bridge.store().revision(), 0);
    }

    #[test]
    fn external_proposals_cannot_claim_human_provenance() {
        let mut bridge = AgentBridge::new(TagStore::default(), None);
        let proposal = Proposal {
            id: "impersonated-human".to_string(),
            based_on_revision: bridge.store().revision(),
            rationale: "test".to_string(),
            provenance: Provenance::human(),
            operations: vec![symtropy_tags::Operation::AddAssertion {
                item: "a.webp".to_string(),
                assertion: TagAssertion::accepted("artist:kitty", Provenance::human()),
            }],
        };
        let response = bridge
            .handle(AgentRequest::ApplyProposal {
                proposal,
                policy: AgentPolicy::Conservative,
            })
            .expect("response");
        match response {
            AgentResponse::Applied { applied, rejected, .. } => {
                assert!(applied.is_empty());
                assert_eq!(rejected.len(), 1);
            }
            other => panic!("unexpected response: {:?}", other),
        }
    }

    #[test]
    fn sidecar_batch_proposals_keep_import_provenance() {
        let mut bridge = AgentBridge::new(TagStore::default(), None);
        let items = vec![ArtistSidecarInput {
            item_id: "a.webp".to_string(),
            tags: vec!["kitty".to_string()],
            artists: vec!["Kitty".to_string()],
            artist_hints: vec![],
        }];
        let response = bridge
            .handle(AgentRequest::ProposeArtistSidecars { items })
            .expect("proposal");
        let proposal = match response {
            AgentResponse::Proposal { proposal, preview } => {
                assert!(preview.rejected.is_empty());
                assert_eq!(preview.planned.len(), 3);
                proposal
            }
            other => panic!("unexpected response: {:?}", other),
        };
        assert_eq!(proposal.provenance.kind, symtropy_tags::ProvenanceKind::Import);
    }

    #[test]
    fn preservation_report_shows_sidecar_only_and_hidden_tags() {
        let mut bridge = AgentBridge::new(TagStore::default(), None);
        let store = bridge.store_mut_for_tests();
        store
            .item_mut("a.webp")
            .upsert_assertion(TagAssertion::accepted("active", Provenance::human()));
        store
            .item_mut("a.webp")
            .upsert_assertion(TagAssertion::rejected("removed", Provenance::human()));

        let items = vec![PreservationSidecarInput {
            item_id: "a.webp".to_string(),
            tags: vec![
                "active".to_string(),
                "removed".to_string(),
                "legacy".to_string(),
            ],
            preserved_tags: vec!["ancient".to_string()],
            artists: vec!["Kitty".to_string()],
            artist_hints: vec![],
            version: 3,
        }];
        let entries = build_preservation_report(bridge.store(), &items, false);
        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.tag == "ancient")
                .map(|entry| entry.status),
            Some(PreservationStatus::Preserved)
        );
        assert!(entries.iter().all(|entry| entry.sidecar_version == 3));
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.tag == "legacy")
                .map(|entry| entry.status),
            Some(PreservationStatus::OnlyInSidecar)
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.tag == "removed")
                .map(|entry| entry.status),
            Some(PreservationStatus::Rejected)
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.tag == "artist:kitty")
                .map(|entry| entry.source),
            Some(PreservationSource::Artist)
        );

        let with_accepted = build_preservation_report(bridge.store(), &items, true);
        assert_eq!(with_accepted.len(), 5);
    }

    #[test]
    fn tag_parent_artist_hints_produce_reviewable_creators() {
        let mut bridge = AgentBridge::new(TagStore::default(), None);
        let items = vec![ArtistSidecarInput {
            item_id: "a.webp".to_string(),
            tags: vec!["miranii".to_string(), "fox".to_string()],
            artists: vec![],
            artist_hints: vec!["miranii".to_string()],
        }];
        let response = bridge
            .handle(AgentRequest::StageArtistSidecars { items })
            .expect("stage");
        match response {
            AgentResponse::Applied { applied, .. } => assert_eq!(applied.len(), 3),
            other => panic!("unexpected response: {:?}", other),
        }

        let item = bridge.store().item("a.webp").expect("item");
        assert_eq!(
            item.assertion("artist:miranii").expect("artist").state,
            AssertionState::Suggested
        );
        assert!(item.assertion("artist:fox").is_none());
    }

    #[test]
    fn staged_suggestions_are_reviewable_and_saved() {
        let path = temp_path("suggestions.json5");
        remove_test_file(&path);
        let mut bridge = AgentBridge::load(&path).expect("bridge");
        let response = bridge
            .handle(AgentRequest::StageSuggestions {
                item_id: "a.webp".to_string(),
                suggestions: vec![
                    AgentSuggestion {
                        tag: "feature:orange".to_string(),
                        confidence: 0.7,
                        reason: "dominant orange pixels".to_string(),
                    },
                    AgentSuggestion {
                        tag: "feature:certain".to_string(),
                        confidence: 1.0,
                        reason: "must not be accepted automatically".to_string(),
                    },
                ],
                tool: "test-agent".to_string(),
                model_version: "v1".to_string(),
            })
            .expect("stage");
        match response {
            AgentResponse::Applied { applied, .. } => assert_eq!(applied.len(), 1),
            other => panic!("unexpected response: {:?}", other),
        }

        let assertion = bridge
            .store()
            .item("a.webp")
            .and_then(|item| item.assertion("feature:orange"))
            .expect("staged assertion");
        assert_eq!(assertion.state, AssertionState::Suggested);
        assert_eq!(assertion.provenance.kind, symtropy_tags::ProvenanceKind::Ai);
        assert!(path.exists());

        let reloaded = TagStore::from_json5(&fs::read_to_string(&path).expect("store text"))
            .expect("reloaded store");
        assert!(reloaded
            .item("a.webp")
            .and_then(|item| item.assertion("feature:orange"))
            .is_some());
        remove_test_file(&path);
    }

    #[test]
    fn explicit_sidecar_artists_become_accepted_creator_metadata() {
        let mut bridge = AgentBridge::new(TagStore::default(), None);
        let response = bridge
            .handle(AgentRequest::StageArtistMetadata {
                item_id: "a.webp".to_string(),
                artists: vec!["Kitty".to_string()],
            })
            .expect("stage");
        match response {
            AgentResponse::Applied { applied, rejected, .. } => {
                assert_eq!(applied.len(), 3);
                assert!(rejected.is_empty());
            }
            other => panic!("unexpected response: {:?}", other),
        }

        let item = bridge.store().item("a.webp").expect("item");
        let artist = item.assertion("artist:kitty").expect("artist");
        assert_eq!(artist.state, AssertionState::Accepted);
        assert_eq!(artist.provenance.kind, symtropy_tags::ProvenanceKind::Import);

        let relations = item.visible_relations(false);
        let creator = relations
            .iter()
            .find(|relation| relation.predicate == RelationPredicate::CreatedBy)
            .expect("created_by relation");
        assert_eq!(creator.state, AssertionState::Accepted);
    }

    #[test]
    fn flat_artist_proposal_only_suggests_and_preserves_original_tag() {
        let mut bridge = AgentBridge::new(TagStore::default(), None);
        bridge.store_mut_for_tests().item_mut("a.webp").upsert_assertion(
            TagAssertion::accepted("kitty", Provenance::import("sidecar")),
        );

        let response = bridge
            .handle(AgentRequest::ProposeArtists {
                item_id: "a.webp".to_string(),
                tags: vec!["kitty".to_string()],
                artist_hints: vec!["kitty".to_string()],
            })
            .expect("proposal");
        let proposal = match response {
            AgentResponse::Proposal { proposal, .. } => proposal,
            other => panic!("unexpected response: {:?}", other),
        };
        let report = bridge
            .store_mut_for_tests()
            .apply_proposal(&proposal, &ApplyPolicy::conservative())
            .expect("apply");
        assert!(report.rejected.is_empty());

        let item = bridge.store().item("a.webp").expect("item");
        assert_eq!(
            item.assertion("kitty").expect("original").state,
            AssertionState::Accepted
        );
        assert_eq!(
            item.assertion("artist:kitty").expect("artist").state,
            AssertionState::Suggested
        );
    }

    #[test]
    fn staged_proposal_definition_uses_creator_kind() {
        let proposal = propose_artist_import(
            "a.webp",
            &["Kitty".to_string()],
            0,
        );
        let definition = proposal.operations.iter().find_map(|operation| match operation {
            symtropy_tags::Operation::DefineTag { definition } => Some(definition),
            _ => None,
        });
        assert_eq!(definition.map(|definition| definition.kind), Some(TagKind::Creator));
    }
}
