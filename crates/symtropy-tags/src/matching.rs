//! Match-gated enrichment from remote sources.
//!
//! This is the reverse direction of the tagging system: local metadata can be
//! used to find a likely remote item, but remote tags only become suggestions
//! after the remote item has been visually matched. They arrive namespaced as
//! `src:<site>:<tag>` evidence, never as canonical local meaning.

use crate::migration::stable_suffix;
use crate::model::*;
use crate::registry::normalize_lookup;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchVerdict {
    Confirmed,
    Probable,
    Rejected,
}

impl MatchVerdict {
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Confirmed | Self::Probable)
    }

    pub fn confidence_factor(self) -> f32 {
        match self {
            Self::Confirmed => 1.0,
            Self::Probable => 0.75,
            Self::Rejected => 0.0,
        }
    }
}

/// Evidence that a remote item is the same image as a local item.
///
/// `similarity` is intentionally generic: an importer may supply exact-byte
/// equality, a perceptual-hash distance, or a pixel comparison. The tag core
/// only decides what a *usable* match is allowed to claim.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchEvidence {
    pub remote_id: String,
    pub source_url: String,
    pub site: String,
    pub similarity: f32,
    pub verdict: MatchVerdict,
    #[serde(default)] pub method: String,
    #[serde(default)] pub observed_tags: Vec<String>,
    #[serde(default)] pub local_dimensions: Option<(u32, u32)>,
    #[serde(default)] pub remote_dimensions: Option<(u32, u32)>,
}

impl MatchEvidence {
    pub fn verdict_for(similarity: f32, confirmed_at: f32, probable_at: f32) -> MatchVerdict {
        if similarity >= confirmed_at {
            MatchVerdict::Confirmed
        } else if similarity >= probable_at {
            MatchVerdict::Probable
        } else {
            MatchVerdict::Rejected
        }
    }

    /// Remote vocabulary stays namespaced. A tag that is already namespaced is
    /// normalized in place instead of being wrapped twice.
    pub fn namespaced_tag(&self, raw: &str) -> TagId {
        let normalized = normalize_lookup(raw);
        if normalized.starts_with("src:") {
            return normalized;
        }
        let site = normalize_lookup(&self.site);
        if site.is_empty() {
            format!("src:unknown:{normalized}")
        } else {
            format!("src:{site}:{normalized}")
        }
    }

    fn confidence(&self) -> f32 {
        (self.similarity.clamp(0.0, 1.0) * self.verdict.confidence_factor()).clamp(0.0, 1.0)
    }
}

/// Build a sandboxed patch from a verified remote match.
///
/// This never emits `Accepted` assertions and never removes local tags. It
/// only adds suggested source-evidence tags and one suggested `evidences`
/// relation pointing back to the remote item.
pub fn propose_enrichment_from_match(
    item_id: &str,
    evidence: &MatchEvidence,
    based_on_revision: u64,
) -> Proposal {
    let mut proposal = Proposal {
        id: format!("match-enrichment-{}", stable_suffix(&evidence.remote_id)),
        based_on_revision,
        rationale: format!(
            "Remote item {} matched local item {} via {} ({:.0}%, {:?}).",
            evidence.remote_id, item_id, evidence.method, evidence.similarity * 100.0, evidence.verdict
        ),
        provenance: Provenance {
            kind: ProvenanceKind::Source,
            tool: format!("match:{}", if evidence.method.is_empty() { "unknown" } else { &evidence.method }),
            source_url: evidence.source_url.clone(),
            note: format!("remote id: {}", evidence.remote_id),
            ..Provenance::default()
        },
        operations: Vec::new(),
    };

    if !evidence.verdict.is_usable() {
        return proposal;
    }

    let provenance = proposal.provenance.clone();
    let confidence = evidence.confidence();
    let evidence_note = format!(
        "matched remote item {} ({}, {:.0}%)",
        evidence.remote_id,
        if evidence.method.is_empty() { "method unknown" } else { &evidence.method },
        evidence.similarity * 100.0
    );

    for raw in &evidence.observed_tags {
        let tag = evidence.namespaced_tag(raw);
        proposal.operations.push(Operation::AddAssertion {
            item: item_id.to_string(),
            assertion: TagAssertion::suggested(tag, provenance.clone(), confidence)
                .with_evidence(evidence_note.clone()),
        });
    }

    let relation_id = format!(
        "match-{}-{}-{}",
        stable_suffix(&evidence.site),
        stable_suffix(&evidence.remote_id),
        stable_suffix(item_id)
    );
    proposal.operations.push(Operation::AddRelation {
        scope: RelationScope::Item(item_id.to_string()),
        relation: TagRelation::new(
            relation_id,
            EntityRef::External(evidence.remote_id.clone()),
            RelationPredicate::Evidences,
            EntityRef::Item(item_id.to_string()),
            AssertionState::Suggested,
            provenance,
            confidence,
        )
        .with_evidence(evidence_note),
    });

    proposal
}
