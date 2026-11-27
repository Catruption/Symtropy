# symtropy-tags

Headless semantic tag core for Symtropy.

This crate is intentionally independent of the UI, filesystem, and network. It is the foundation for:

- stable tag definitions and aliases,
- first-class typed relations,
- per-image tag assertions with provenance/confidence/evidence,
- sandboxed AI proposals,
- migration from Symtropy's current flat tag sidecars.

## Model

### Definitions

A `TagDefinition` is the stable meaning of a tag:

```text
id: sym:idea:claimed_possession
label: Claimed possession
kind: idea
parent: theme:ownership
aliases: [possession(idea)]
version: 1
```

### Assertions

An item does not simply "have tags". It has `TagAssertion`s:

```text
tag: artist:kitty
state: suggested
confidence: 0.75
provenance: computed:legacy-artist-conversion
evidence: legacy tag looked like an artist name
locked: false
```

Accepted assertions feed the flat `tags[]` projection used by the existing query engine. Suggested assertions are visible to review UI but are not authoritative.

### Relations

Non-tree connections are `TagRelation`s:

```text
item:a.webp --created_by--> artist:kitty
act:possession --realizes--> sym:idea:claimed_possession
part:wing --part_of--> taxon:bird
```

Relations have their own state, confidence, provenance, evidence, and version. They are never deleted silently; they can be marked `superseded`.

## AI sandboxing

AI output is a `Proposal`, not a mutation.

```rust
let preview = store.preview(&proposal, &ApplyPolicy::conservative());
let report = store.apply_proposal(&proposal, &ApplyPolicy::conservative())?;
```

The conservative policy:

- rejects stale proposals,
- requires minimum confidence,
- never lets machine output overwrite human-accepted tags,
- never lets machine output overwrite locked tags.

This makes it safe to ask an AI to "turn these relations into this better one": the result is a patch you can review and apply selectively.

## Match-gated remote enrichment

The reverse direction is also proposal-based:

```text
local tags → user-initiated site search → candidate posts
           → visual match confirmation → src:<site>:<tag> suggestions
```

`MatchEvidence` records how a remote item was matched. `propose_enrichment_from_match` only emits:

- `Suggested` `src:<site>:<tag>` assertions,
- one `Suggested` `evidences` relation back to the remote item.

Rejected matches produce no operations. Confirmed matches never create accepted tags and never remove local tags. This keeps source-site vocabulary as evidence instead of silently rewriting the local taxonomy.

## Migration

`migrate_flat_tags` preserves every legacy tag as an accepted imported assertion. Artist-looking tags additionally receive a suggested `artist:` assertion and a suggested `created_by` relation.

Nothing is removed. Nothing is auto-promoted to a fact.
