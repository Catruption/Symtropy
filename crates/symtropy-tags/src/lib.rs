#![forbid(unsafe_code)]
//! Headless semantic tag core for Symtropy.
//!
//! This crate intentionally knows nothing about egui, filesystems, or HTTP.
//! It models tags as:
//!
//! - [`TagDefinition`]: stable meaning, alias, kind, parent, version.
//! - [`TagAssertion`]: a per-item claim with state, confidence, provenance,
//!   evidence, and locking.
//! - [`TagRelation`]: a first-class typed edge between tags, items, or external
//!   identities.
//! - [`Proposal`]: a sandboxed patch produced by AI or a migration tool.
//!
//! AI output never mutates the store directly. Use [`TagStore::preview`] then
//! [`TagStore::apply_proposal`] with an explicit [`ApplyPolicy`].

mod matching;
mod migration;
mod model;
mod proposals;
mod registry;
mod store;

pub use matching::{propose_enrichment_from_match, MatchEvidence, MatchVerdict};
pub use migration::{
    artist_definition, artist_tag_id, is_artist_candidate, migrate_flat_tags, stable_suffix,
};
pub use model::*;
pub use proposals::{propose_artist_conversion, propose_relation_conversion};
pub use registry::{normalize_lookup, TagRegistry};
pub use store::TagStore;

#[cfg(test)]
mod tests {
    use super::*;

    fn ai(tool: &str) -> Provenance {
        Provenance::ai(tool, "test-model-v1")
    }

    #[test]
    fn legacy_tags_survive_and_artist_metadata_is_only_suggested() {
        let item = migrate_flat_tags(
            "cat.webp",
            &["kitty".to_string(), "fox".to_string()],
            &["kitty".to_string()],
        );

        let projected = item.projection(false);
        assert_eq!(projected, vec!["fox".to_string(), "kitty".to_string()]);
        assert_eq!(
            item.projection(true),
            vec!["artist:kitty".to_string(), "fox".to_string(), "kitty".to_string()]
        );

        let artist = item.assertion("artist:kitty").expect("artist assertion");
        assert_eq!(artist.state, AssertionState::Suggested);
        assert!(artist.provenance.is_machine());

        let relation = item
            .relation("cat_webp-created-by-artist_kitty")
            .expect("created_by relation");
        assert_eq!(relation.predicate, RelationPredicate::CreatedBy);
        assert_eq!(relation.state, AssertionState::Suggested);
    }

    #[test]
    fn alias_lookup_and_hierarchy_work() {
        let mut registry = TagRegistry::default();
        registry
            .insert(TagDefinition::new("taxon:animal", "Animal", TagKind::Taxon))
            .unwrap();
        registry
            .insert(
                TagDefinition::new("taxon:domestic_cat", "Domestic cat", TagKind::Taxon)
                    .with_parent("taxon:animal")
                    .with_alias("house cat"),
            )
            .unwrap();

        assert_eq!(
            registry.resolve("HOUSE CAT").map(|def| def.id.as_str()),
            Some("taxon:domestic_cat")
        );
        assert!(registry.is_descendant_of("taxon:domestic_cat", "taxon:animal"));
        assert!(!registry.is_descendant_of("taxon:animal", "taxon:domestic_cat"));
    }

    #[test]
    fn hierarchy_cycles_are_rejected() {
        let mut registry = TagRegistry::default();
        registry
            .insert(TagDefinition::new("a", "A", TagKind::Idea))
            .unwrap();
        registry
            .insert(TagDefinition::new("b", "B", TagKind::Idea).with_parent("a"))
            .unwrap();
        registry
            .insert(TagDefinition::new("c", "C", TagKind::Idea).with_parent("b"))
            .unwrap();

        // Directly rewriting is not public API, so insert an equivalent cycle
        // by redefining the root with a child as its parent.
        registry
            .insert(TagDefinition::new("a", "A", TagKind::Idea).with_parent("c"))
            .unwrap();
        assert!(registry.validate().is_err());
    }

    #[test]
    fn conservative_policy_does_not_let_ai_overwrite_locked_human_tags() {
        let mut store = TagStore::default();
        store
            .registry_mut()
            .insert(TagDefinition::new("idea:claim", "Claim", TagKind::Idea))
            .unwrap();
        store.item_mut("a.webp").upsert_assertion(
            TagAssertion::accepted("idea:claim", Provenance::human()).locked(true),
        );

        let proposal = Proposal {
            id: "ai-overwrite".to_string(),
            based_on_revision: store.revision(),
            provenance: ai("tag-suggester"),
            operations: vec![Operation::AddAssertion {
                item: "a.webp".to_string(),
                assertion: TagAssertion::suggested("idea:claim", ai("tag-suggester"), 0.99)
                    .with_evidence("looks related"),
            }],
            ..Proposal::default()
        };

        let preview = store.preview(&proposal, &ApplyPolicy::conservative());
        assert!(preview.planned.is_empty());
        assert_eq!(preview.rejected.len(), 1);

        let report = store
            .apply_proposal(&proposal, &ApplyPolicy::conservative())
            .unwrap();
        assert!(report.applied.is_empty());
        assert_eq!(store.revision(), 0);
        assert_eq!(
            store.item("a.webp").unwrap().assertion("idea:claim").unwrap().state,
            AssertionState::Accepted
        );
    }

    #[test]
    fn conservative_policy_refuses_machine_created_accepted_claims() {
        let mut store = TagStore::default();
        let proposal = Proposal {
            id: "ai-accepted".to_string(),
            based_on_revision: store.revision(),
            provenance: ai("tag-suggester"),
            operations: vec![Operation::AddAssertion {
                item: "a.webp".to_string(),
                assertion: TagAssertion::accepted("artist:someone", ai("tag-suggester")),
            }],
            ..Proposal::default()
        };

        let preview = store.preview(&proposal, &ApplyPolicy::conservative());
        assert!(preview.planned.is_empty());
        assert_eq!(preview.rejected.len(), 1);

        let report = store
            .apply_proposal(&proposal, &ApplyPolicy::conservative())
            .unwrap();
        assert!(report.applied.is_empty());
        assert!(store.item("a.webp").is_none() || store.item("a.webp").unwrap().assertions.is_empty());
    }

    #[test]
    fn relation_conversion_supersedes_old_edges_without_deleting_them() {
        let mut store = TagStore::default();
        store
            .registry_mut()
            .insert(TagDefinition::new("artist:kitty", "Kitty", TagKind::Creator))
            .unwrap();
        store.item_mut("a.webp").upsert_relation(TagRelation::new(
            "old-artist-guess",
            EntityRef::Item("a.webp".to_string()),
            RelationPredicate::RelatedTo,
            EntityRef::External("kitty".to_string()),
            AssertionState::Accepted,
            Provenance::import("legacy"),
            1.0,
        ));

        let replacement = TagRelation::new(
            "new-artist-creator",
            EntityRef::Item("a.webp".to_string()),
            RelationPredicate::CreatedBy,
            EntityRef::Tag("artist:kitty".to_string()),
            AssertionState::Accepted,
            Provenance::human(),
            1.0,
        );
        let proposal = propose_relation_conversion(
            RelationScope::Item("a.webp".to_string()),
            &["old-artist-guess".to_string()],
            replacement,
            store.revision(),
            "turn vague artist guess into creator metadata",
        );

        let report = store.apply_proposal(&proposal, &ApplyPolicy::default()).unwrap();
        assert!(report.rejected.is_empty());
        assert_eq!(report.applied.len(), 2);

        let item = store.item("a.webp").unwrap();
        let old = item.relation("old-artist-guess").unwrap();
        assert_eq!(old.state, AssertionState::Superseded);
        assert_eq!(old.superseded_by.as_deref(), Some("new-artist-creator"));
        assert_eq!(
            item.relation("new-artist-creator").unwrap().predicate,
            RelationPredicate::CreatedBy
        );
    }

    #[test]
    fn store_json5_round_trip_preserves_definitions_and_assertions() {
        let mut store = TagStore::default();
        store
            .registry_mut()
            .insert(
                TagDefinition::new("sym:idea:claim", "Claimed", TagKind::Idea)
                    .with_alias("claimed possession"),
            )
            .unwrap();
        store.item_mut("a.webp").upsert_assertion(
            TagAssertion::accepted("sym:idea:claim", Provenance::human())
                .with_evidence("manual review"),
        );

        let text = store.to_json5_pretty().unwrap();
        let loaded = TagStore::from_json5(&text).unwrap();

        assert!(loaded.registry().resolve("claimed possession").is_some());
        assert_eq!(
            loaded.item("a.webp").unwrap().assertion("sym:idea:claim").unwrap().state,
            AssertionState::Accepted
        );
    }

    #[test]
    fn rejected_match_produces_no_suggestions() {
        let evidence = MatchEvidence {
            remote_id: "post-123".to_string(),
            source_url: "https://example.invalid/post/123".to_string(),
            site: "e621".to_string(),
            similarity: 0.31,
            verdict: MatchVerdict::Rejected,
            method: "dhash".to_string(),
            observed_tags: vec!["possession".to_string()],
            local_dimensions: Some((800, 600)),
            remote_dimensions: Some((1024, 768)),
        };
        let proposal = propose_enrichment_from_match("a.webp", &evidence, 0);
        assert!(proposal.operations.is_empty());
    }

    #[test]
    fn confirmed_match_only_adds_namespaced_suggestions() {
        let mut store = TagStore::default();
        store.item_mut("a.webp").upsert_assertion(
            TagAssertion::accepted("sym:idea:claimed_possession", Provenance::human()),
        );

        let evidence = MatchEvidence {
            remote_id: "post-123".to_string(),
            source_url: "https://example.invalid/post/123".to_string(),
            site: "e621".to_string(),
            similarity: 0.98,
            verdict: MatchVerdict::Confirmed,
            method: "dhash".to_string(),
            observed_tags: vec![
                "possession".to_string(),
                "src:e621:tail".to_string(),
            ],
            local_dimensions: Some((800, 600)),
            remote_dimensions: Some((800, 600)),
        };
        let proposal = propose_enrichment_from_match("a.webp", &evidence, store.revision());
        assert!(proposal.operations.iter().all(|operation| match operation {
            Operation::AddAssertion { assertion, .. } => {
                assertion.state == AssertionState::Suggested
                    && assertion.provenance.kind == ProvenanceKind::Source
            }
            Operation::AddRelation { relation, .. } => {
                relation.state == AssertionState::Suggested
                    && relation.predicate == RelationPredicate::Evidences
            }
            _ => false,
        }));

        let report = store
            .apply_proposal(&proposal, &ApplyPolicy::conservative())
            .unwrap();
        assert!(report.rejected.is_empty());
        assert_eq!(report.applied.len(), 3);

        let item = store.item("a.webp").unwrap();
        assert_eq!(
            item.assertion("src:e621:possession").unwrap().state,
            AssertionState::Suggested
        );
        assert_eq!(
            item.assertion("src:e621:tail").unwrap().state,
            AssertionState::Suggested
        );
        assert_eq!(
            item.assertion("sym:idea:claimed_possession").unwrap().state,
            AssertionState::Accepted
        );
        assert_eq!(item.projection(false), vec!["sym:idea:claimed_possession".to_string()]);
    }

    #[test]
    fn similarity_thresholds_produce_the_expected_verdict() {
        assert_eq!(MatchEvidence::verdict_for(0.99, 0.95, 0.8), MatchVerdict::Confirmed);
        assert_eq!(MatchEvidence::verdict_for(0.85, 0.95, 0.8), MatchVerdict::Probable);
        assert_eq!(MatchEvidence::verdict_for(0.5, 0.95, 0.8), MatchVerdict::Rejected);
    }

    #[test]
    fn stale_proposals_are_rejected() {
        let mut store = TagStore::default();
        let proposal = Proposal {
            id: "stale".to_string(),
            based_on_revision: 99,
            ..Proposal::default()
        };
        let preview = store.preview(&proposal, &ApplyPolicy::default());
        assert_eq!(preview.rejected.len(), 1);
        assert!(store.apply_proposal(&proposal, &ApplyPolicy::default()).is_err());
    }
}
