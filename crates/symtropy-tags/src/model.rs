//! Core data model for Symtropy's semantic tag system.
//!
//! A tag is not just a string. A tag *definition* has stable identity and
//! meaning; an item's tag is an *assertion* with provenance and confidence;
//! non-tree connections are *relations*. AI output is represented as a proposal
//! that can be previewed and applied under an explicit policy.

use serde::{Deserialize, Serialize};

pub type TagId = String;
pub type ItemId = String;

pub fn default_version() -> u32 {
    1
}

pub fn full_confidence() -> f32 {
    1.0
}

pub fn zero_confidence() -> f32 {
    0.0
}

/// What kind of meaning a tag has.
///
/// `Creator` is the canonical home for artist/author names. The `artist:`
/// prefix is a conventional ID prefix, not a separate type.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TagKind {
    Unknown,
    Entity,
    Taxon,
    Anatomy,
    Act,
    State,
    Role,
    Theme,
    Idea,
    Creator,
    Cue,
    Source,
    Feature,
    Meta,
}

impl Default for TagKind {
    fn default() -> Self {
        Self::Unknown
    }
}

impl TagKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Entity => "entity",
            Self::Taxon => "taxon",
            Self::Anatomy => "anatomy",
            Self::Act => "act",
            Self::State => "state",
            Self::Role => "role",
            Self::Theme => "theme",
            Self::Idea => "idea",
            Self::Creator => "creator",
            Self::Cue => "cue",
            Self::Source => "source",
            Self::Feature => "feature",
            Self::Meta => "meta",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TagStatus {
    #[default]
    Active,
    Proposed,
    Deprecated,
}

/// Where a claim came from. This is the barrier between human facts and
/// non-authoritative guesses: generated, computed, and external-source
/// provenance cannot silently replace accepted or locked local decisions.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceKind {
    #[default]
    Unknown,
    Human,
    Source,
    Import,
    Computed,
    Ai,
}

impl ProvenanceKind {
    pub fn is_machine(self) -> bool {
        matches!(self, Self::Source | Self::Computed | Self::Ai)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Provenance {
    #[serde(default)]
    pub kind: ProvenanceKind,
    #[serde(default)] pub tool: String,
    #[serde(default)] pub model_version: String,
    #[serde(default)] pub source_url: String,
    #[serde(default)] pub captured_at: String,
    #[serde(default)] pub note: String,
}

impl Provenance {
    pub fn human() -> Self {
        Self { kind: ProvenanceKind::Human, ..Self::default() }
    }

    pub fn import(tool: impl Into<String>) -> Self {
        Self { kind: ProvenanceKind::Import, tool: tool.into(), ..Self::default() }
    }

    pub fn source(url: impl Into<String>) -> Self {
        Self { kind: ProvenanceKind::Source, source_url: url.into(), ..Self::default() }
    }

    pub fn computed(tool: impl Into<String>) -> Self {
        Self { kind: ProvenanceKind::Computed, tool: tool.into(), ..Self::default() }
    }

    pub fn ai(tool: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            kind: ProvenanceKind::Ai,
            tool: tool.into(),
            model_version: model.into(),
            ..Self::default()
        }
    }

    pub fn is_machine(&self) -> bool {
        self.kind.is_machine()
    }
}

/// Global meaning of a tag. IDs are stable; labels and aliases may change.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagDefinition {
    pub id: TagId,
    pub label: String,
    #[serde(default)] pub kind: TagKind,
    #[serde(default)] pub definition: String,
    #[serde(default)] pub aliases: Vec<String>,
    #[serde(default)] pub parent: Option<TagId>,
    #[serde(default)] pub status: TagStatus,
    #[serde(default = "default_version")] pub version: u32,
    #[serde(default)] pub provenance: Provenance,
}

impl TagDefinition {
    pub fn new(id: impl Into<TagId>, label: impl Into<String>, kind: TagKind) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind,
            definition: String::new(),
            aliases: Vec::new(),
            parent: None,
            status: TagStatus::Active,
            version: default_version(),
            provenance: Provenance::human(),
        }
    }

    pub fn with_parent(mut self, parent: impl Into<TagId>) -> Self {
        self.parent = Some(parent.into());
        self
    }

    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    pub fn with_definition(mut self, definition: impl Into<String>) -> Self {
        self.definition = definition.into();
        self
    }

    pub fn with_provenance(mut self, provenance: Provenance) -> Self {
        self.provenance = provenance;
        self
    }

    pub fn with_status(mut self, status: TagStatus) -> Self {
        self.status = status;
        self
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssertionState {
    Accepted,
    #[default]
    Suggested,
    Rejected,
    Superseded,
}

impl AssertionState {
    pub fn is_visible(self) -> bool {
        matches!(self, Self::Accepted | Self::Suggested)
    }
}

/// One item's claim that a tag applies. This is the per-file authority layer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagAssertion {
    pub tag: TagId,
    #[serde(default)] pub state: AssertionState,
    #[serde(default = "full_confidence")] pub confidence: f32,
    #[serde(default)] pub provenance: Provenance,
    #[serde(default)] pub evidence: Vec<String>,
    #[serde(default)] pub locked: bool,
    #[serde(default = "default_version")] pub version: u32,
    #[serde(default)] pub superseded_by: Option<TagId>,
}

impl TagAssertion {
    pub fn new(tag: impl Into<TagId>, state: AssertionState, provenance: Provenance, confidence: f32) -> Self {
        Self {
            tag: tag.into(),
            state,
            confidence: confidence.clamp(0.0, 1.0),
            provenance,
            evidence: Vec::new(),
            locked: false,
            version: default_version(),
            superseded_by: None,
        }
    }

    pub fn accepted(tag: impl Into<TagId>, provenance: Provenance) -> Self {
        Self::new(tag, AssertionState::Accepted, provenance, 1.0)
    }

    pub fn suggested(tag: impl Into<TagId>, provenance: Provenance, confidence: f32) -> Self {
        Self::new(tag, AssertionState::Suggested, provenance, confidence)
    }

    pub fn rejected(tag: impl Into<TagId>, provenance: Provenance) -> Self {
        Self::new(tag, AssertionState::Rejected, provenance, 0.0)
    }

    pub fn with_evidence(mut self, evidence: impl Into<String>) -> Self {
        self.evidence.push(evidence.into());
        self
    }

    pub fn locked(mut self, locked: bool) -> Self {
        self.locked = locked;
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum EntityRef {
    Tag(TagId),
    Item(ItemId),
    External(String),
}

impl EntityRef {
    pub fn key(&self) -> String {
        match self {
            Self::Tag(id) => format!("tag:{id}"),
            Self::Item(id) => format!("item:{id}"),
            Self::External(value) => format!("external:{value}"),
        }
    }

    pub fn as_tag(&self) -> Option<&str> {
        match self {
            Self::Tag(id) => Some(id),
            _ => None,
        }
    }

    /// Human-readable form without the internal `tag:`/`item:` prefixes.
    pub fn display(&self) -> String {
        match self {
            Self::Tag(id) | Self::Item(id) | Self::External(id) => id.clone(),
        }
    }
}

/// Typed relation vocabulary. `related_to` is the generic fallback.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RelationPredicate {
    RelatedTo,
    SubclassOf,
    PartOf,
    MemberOf,
    MapsTo,
    Aliases,
    Realizes,
    Evidences,
    AdjacentTo,
    ContrastsWith,
    RequiresContext,
    ImpliesCue,
    CreatedBy,
    Depicts,
    LocatedAt,
    InspiredBy,
    DerivedFrom,
}

impl Default for RelationPredicate {
    fn default() -> Self {
        Self::RelatedTo
    }
}

impl RelationPredicate {
    pub fn parse(value: &str) -> Self {
        let normalized = crate::normalize_lookup(value);
        match normalized.as_str() {
            "subclass_of" => Self::SubclassOf,
            "part_of" => Self::PartOf,
            "member_of" => Self::MemberOf,
            "maps_to" => Self::MapsTo,
            "aliases" => Self::Aliases,
            "realizes" => Self::Realizes,
            "evidences" => Self::Evidences,
            "adjacent_to" => Self::AdjacentTo,
            "contrasts_with" => Self::ContrastsWith,
            "requires_context" => Self::RequiresContext,
            "implies_cue" => Self::ImpliesCue,
            "created_by" => Self::CreatedBy,
            "depicts" => Self::Depicts,
            "located_at" => Self::LocatedAt,
            "inspired_by" => Self::InspiredBy,
            "derived_from" => Self::DerivedFrom,
            _ => Self::RelatedTo,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::RelatedTo => "related_to",
            Self::SubclassOf => "subclass_of",
            Self::PartOf => "part_of",
            Self::MemberOf => "member_of",
            Self::MapsTo => "maps_to",
            Self::Aliases => "aliases",
            Self::Realizes => "realizes",
            Self::Evidences => "evidences",
            Self::AdjacentTo => "adjacent_to",
            Self::ContrastsWith => "contrasts_with",
            Self::RequiresContext => "requires_context",
            Self::ImpliesCue => "implies_cue",
            Self::CreatedBy => "created_by",
            Self::Depicts => "depicts",
            Self::LocatedAt => "located_at",
            Self::InspiredBy => "inspired_by",
            Self::DerivedFrom => "derived_from",
        }
    }
}

/// First-class semantic edge. This is not stored as a plain tag.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagRelation {
    pub id: String,
    pub subject: EntityRef,
    #[serde(default)] pub predicate: RelationPredicate,
    pub object: EntityRef,
    #[serde(default)] pub state: AssertionState,
    #[serde(default = "full_confidence")] pub confidence: f32,
    #[serde(default)] pub provenance: Provenance,
    #[serde(default)] pub evidence: Vec<String>,
    #[serde(default)] pub locked: bool,
    #[serde(default = "default_version")] pub version: u32,
    #[serde(default)] pub superseded_by: Option<String>,
}

impl TagRelation {
    pub fn new(
        id: impl Into<String>,
        subject: EntityRef,
        predicate: RelationPredicate,
        object: EntityRef,
        state: AssertionState,
        provenance: Provenance,
        confidence: f32,
    ) -> Self {
        Self {
            id: id.into(),
            subject,
            predicate,
            object,
            state,
            confidence: confidence.clamp(0.0, 1.0),
            provenance,
            evidence: Vec::new(),
            locked: false,
            version: default_version(),
            superseded_by: None,
        }
    }

    pub fn with_evidence(mut self, evidence: impl Into<String>) -> Self {
        self.evidence.push(evidence.into());
        self
    }

    pub fn locked(mut self, locked: bool) -> Self {
        self.locked = locked;
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ItemTags {
    #[serde(default)] pub item_id: ItemId,
    #[serde(default)] pub assertions: Vec<TagAssertion>,
    #[serde(default)] pub relations: Vec<TagRelation>,
}

impl ItemTags {
    pub fn new(item_id: impl Into<ItemId>) -> Self {
        Self { item_id: item_id.into(), assertions: Vec::new(), relations: Vec::new() }
    }

    pub fn assertion(&self, tag: &str) -> Option<&TagAssertion> {
        self.assertions.iter().find(|a| a.tag == tag)
    }

    pub fn assertion_mut(&mut self, tag: &str) -> Option<&mut TagAssertion> {
        self.assertions.iter_mut().find(|a| a.tag == tag)
    }

    pub fn upsert_assertion(&mut self, assertion: TagAssertion) {
        if let Some(existing) = self.assertion_mut(&assertion.tag) {
            *existing = assertion;
        } else {
            self.assertions.push(assertion);
        }
        self.assertions.sort_by(|a, b| a.tag.cmp(&b.tag));
    }

    pub fn relation(&self, id: &str) -> Option<&TagRelation> {
        self.relations.iter().find(|r| r.id == id)
    }

    pub fn relation_mut(&mut self, id: &str) -> Option<&mut TagRelation> {
        self.relations.iter_mut().find(|r| r.id == id)
    }

    pub fn upsert_relation(&mut self, relation: TagRelation) {
        if let Some(existing) = self.relation_mut(&relation.id) {
            *existing = relation;
        } else {
            self.relations.push(relation);
        }
        self.relations.sort_by(|a, b| a.id.cmp(&b.id));
    }

    /// The flat tag list used by the current query engine. Accepted tags are
    /// always included; suggested tags are opt-in. Rejected and superseded
    /// tags are never included.
    pub fn projection(&self, include_suggested: bool) -> Vec<TagId> {
        let mut out: Vec<TagId> = self
            .assertions
            .iter()
            .filter(|a| a.state == AssertionState::Accepted || (include_suggested && a.state == AssertionState::Suggested))
            .map(|a| a.tag.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn visible_relations(&self, include_suggested: bool) -> Vec<&TagRelation> {
        self.relations
            .iter()
            .filter(|r| r.state == AssertionState::Accepted || (include_suggested && r.state == AssertionState::Suggested))
            .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelationScope {
    Global,
    Item(ItemId),
}

impl RelationScope {
    pub fn key(&self) -> String {
        match self {
            Self::Global => "global".to_string(),
            Self::Item(id) => format!("item:{id}"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    DefineTag { definition: TagDefinition },
    AddAssertion { item: ItemId, assertion: TagAssertion },
    SetAssertionState {
        item: ItemId,
        tag: TagId,
        state: AssertionState,
        #[serde(default)] superseded_by: Option<TagId>,
        #[serde(default)] provenance: Provenance,
        #[serde(default)] evidence: Vec<String>,
    },
    AddRelation { scope: RelationScope, relation: TagRelation },
    SetRelationState {
        scope: RelationScope,
        relation_id: String,
        state: AssertionState,
        #[serde(default)] superseded_by: Option<String>,
        #[serde(default)] provenance: Provenance,
        #[serde(default)] evidence: Vec<String>,
    },
}

/// A sandboxed batch of changes. Nothing here mutates the store until it is
/// explicitly applied through [`crate::TagStore::apply_proposal`].
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Proposal {
    pub id: String,
    #[serde(default)] pub based_on_revision: u64,
    #[serde(default)] pub rationale: String,
    #[serde(default)] pub provenance: Provenance,
    #[serde(default)] pub operations: Vec<Operation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApplyPolicy {
    #[serde(default = "default_true")] pub strict_revision: bool,
    #[serde(default = "zero_confidence")] pub min_confidence: f32,
    #[serde(default)] pub allow_machine_overwrite_accepted: bool,
    #[serde(default)] pub allow_machine_overwrite_locked: bool,
}

fn default_true() -> bool {
    true
}

impl Default for ApplyPolicy {
    fn default() -> Self {
        Self {
            strict_revision: true,
            min_confidence: 0.0,
            allow_machine_overwrite_accepted: false,
            allow_machine_overwrite_locked: false,
        }
    }
}

impl ApplyPolicy {
    /// Conservative policy for AI output: never overwrite human/hard facts.
    pub fn conservative() -> Self {
        Self {
            strict_revision: true,
            min_confidence: 0.5,
            allow_machine_overwrite_accepted: false,
            allow_machine_overwrite_locked: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Change {
    pub op: usize,
    pub kind: String,
    pub target: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RejectedChange {
    pub op: usize,
    pub kind: String,
    pub target: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ProposalPreview {
    pub proposal_id: String,
    pub revision: u64,
    pub planned: Vec<Change>,
    pub rejected: Vec<RejectedChange>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ApplyReport {
    pub proposal_id: String,
    pub old_revision: u64,
    pub new_revision: u64,
    pub applied: Vec<Change>,
    pub rejected: Vec<RejectedChange>,
}
