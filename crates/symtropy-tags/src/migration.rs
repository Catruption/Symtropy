//! Migration from the current flat tag model.
//!
//! Legacy tags are preserved as accepted imported assertions so nothing
//! disappears. Artist conversion is only suggested: a flat string is evidence,
//! not proof that the string is a person.

use crate::model::*;
use crate::registry::normalize_lookup;

pub fn artist_tag_id(raw: &str) -> TagId {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    let bare = lower
        .strip_prefix("artist:")
        .or_else(|| lower.strip_prefix("creator:"))
        .unwrap_or(trimmed);
    format!("artist:{}", normalize_lookup(bare))
}

pub fn is_artist_candidate(raw: &str, artist_hints: &[String]) -> bool {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.starts_with("artist:") || lower.starts_with("creator:") {
        return true;
    }
    let key = normalize_lookup(raw);
    !key.is_empty() && artist_hints.iter().any(|hint| normalize_lookup(hint) == key)
}

pub fn artist_definition(raw: &str) -> TagDefinition {
    let id = artist_tag_id(raw);
    let label = raw
        .trim()
        .strip_prefix("artist:")
        .or_else(|| raw.trim().strip_prefix("creator:"))
        .unwrap_or(raw.trim())
        .to_string();
    let label = if label.is_empty() { "Unknown artist".to_string() } else { label };
    TagDefinition::new(id, label, TagKind::Creator)
        .with_definition("Artist/creator identity converted from a legacy tag.")
        .with_status(TagStatus::Proposed)
        .with_provenance(Provenance::computed("legacy-artist-conversion"))
}

pub fn stable_suffix(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect()
}

/// Convert legacy flat tags without losing anything.
///
/// - Every old tag stays as an accepted `Import` assertion.
/// - Suspected artist names also gain a suggested `artist:` assertion and a
///   suggested `created_by` relation. Those are not authoritative.
pub fn migrate_flat_tags(item_id: &str, tags: &[String], artist_hints: &[String]) -> ItemTags {
    let mut item = ItemTags::new(item_id);

    for tag in tags {
        let trimmed = tag.trim();
        if trimmed.is_empty() {
            continue;
        }
        item.upsert_assertion(
            TagAssertion::accepted(trimmed, Provenance::import("legacy-flat-tags"))
                .with_evidence("migrated from legacy flat tag list"),
        );
    }

    for tag in tags {
        if !is_artist_candidate(tag, artist_hints) {
            continue;
        }
        let artist = artist_tag_id(tag);
        if item.assertion(&artist).is_none() {
            item.upsert_assertion(
                TagAssertion::suggested(
                    artist.clone(),
                    Provenance::computed("legacy-artist-conversion"),
                    0.75,
                )
                .with_evidence(format!("legacy tag looked like an artist name: {}", tag.trim())),
            );
        }
        let relation_id = format!("{}-created-by-{}", stable_suffix(item_id), stable_suffix(&artist));
        item.upsert_relation(
            TagRelation::new(
                relation_id,
                EntityRef::Item(item_id.to_string()),
                RelationPredicate::CreatedBy,
                EntityRef::Tag(artist),
                AssertionState::Suggested,
                Provenance::computed("legacy-artist-conversion"),
                0.75,
            )
            .with_evidence(format!("derived from legacy tag: {}", tag.trim())),
        );
    }

    item
}
