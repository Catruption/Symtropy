//! Helpers for building sandboxed proposals.
//!
//! These functions never mutate a [`crate::TagStore`]. They return operations
//! that must be previewed and applied explicitly.

use crate::migration::{artist_definition, artist_tag_id, is_artist_candidate};
use crate::model::*;

/// Suggest turning legacy artist-looking tags into creator metadata.
///
/// The original tags are not removed. Every generated assertion/relation is
/// `Suggested`, so the conservative apply policy will add it as a soft claim
/// without overwriting a human-accepted fact.
pub fn propose_artist_conversion(
    item_id: &str,
    tags: &[String],
    artist_hints: &[String],
    based_on_revision: u64,
) -> Proposal {
    let mut proposal = Proposal {
        id: format!("artist-conversion-{}", crate::migration::stable_suffix(item_id)),
        based_on_revision,
        rationale: "Convert artist-looking legacy tags into creator metadata as suggestions.".to_string(),
        provenance: Provenance::computed("legacy-artist-conversion"),
        operations: Vec::new(),
    };

    for tag in tags {
        if !is_artist_candidate(tag, artist_hints) {
            continue;
        }
        let artist = artist_tag_id(tag);
        proposal.operations.push(Operation::DefineTag {
            definition: artist_definition(tag),
        });
        proposal.operations.push(Operation::AddAssertion {
            item: item_id.to_string(),
            assertion: TagAssertion::suggested(
                artist.clone(),
                Provenance::computed("legacy-artist-conversion"),
                0.75,
            )
            .with_evidence(format!("legacy tag looked like an artist name: {}", tag.trim())),
        });
        let relation_id = format!(
            "{}-created-by-{}",
            crate::migration::stable_suffix(item_id),
            crate::migration::stable_suffix(&artist)
        );
        proposal.operations.push(Operation::AddRelation {
            scope: RelationScope::Item(item_id.to_string()),
            relation: TagRelation::new(
                relation_id,
                EntityRef::Item(item_id.to_string()),
                RelationPredicate::CreatedBy,
                EntityRef::Tag(artist),
                AssertionState::Suggested,
                Provenance::computed("legacy-artist-conversion"),
                0.75,
            ),
        });
    }

    proposal
}

/// A proposed relation cleanup: mark old edges as superseded and add one
/// replacement edge. Nothing is deleted, so the reasoning stays auditable.
pub fn propose_relation_conversion(
    scope: RelationScope,
    source_relation_ids: &[String],
    replacement: TagRelation,
    based_on_revision: u64,
    rationale: impl Into<String>,
) -> Proposal {
    let mut operations = Vec::new();
    let provenance = replacement.provenance.clone();
    let target = replacement.id.clone();

    for id in source_relation_ids {
        operations.push(Operation::SetRelationState {
            scope: scope.clone(),
            relation_id: id.clone(),
            state: AssertionState::Superseded,
            superseded_by: Some(target.clone()),
            provenance: provenance.clone(),
            evidence: vec![format!("superseded by relation '{}'", target)],
        });
    }
    operations.push(Operation::AddRelation {
        scope,
        relation: replacement,
    });

    Proposal {
        id: format!("relation-conversion-{}", crate::migration::stable_suffix(&target)),
        based_on_revision,
        rationale: rationale.into(),
        provenance,
        operations,
    }
}
