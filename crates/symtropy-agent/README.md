# symtropy-agent

A local, proposal-only bridge between external agents and Symtropy's tag system.

It does not use the network and it never gives an agent raw write access to the
tag store. Every mutation becomes a `symtropy_tags::Proposal`, is checked with
a safe `ApplyPolicy`, and is atomically saved only after a successful apply.

## Safe surface

Agents can:

- read store status,
- preview a proposal,
- apply a proposal that was previously generated or written by the agent,
- ask for an artist-conversion proposal from flat tags,
- stage explicit sidecar artist fields as creator metadata,
- stage a whole sidecar batch through the dedicated `stage_artist_sidecars` request,
- build read-only preservation reports from sidecar metadata,
- stage arbitrary AI/agent suggestions as reviewable `Suggested` claims.

Agents cannot:

- overwrite accepted or locked human claims,
- create accepted machine claims,
- bypass revision checks,
- claim Human/Import provenance through `apply_proposal` — external proposals
  are downgraded to untrusted AI provenance first,
- choose the unsafe `allow_machine_overwrite_*` policy flags,
- delete tags or relations.

## CLI

Build:

```sh
cargo build --bin symtropy-agent
```

Status:

```sh
./target/debug/symtropy-agent status
```

Generic agent request, JSON5 on stdin:

```sh
cat request.json5 | ./target/debug/symtropy-agent request
```

Example request:

```json5
{
  kind: 'stage_suggestions',
  item_id: '/library/photos/fox.webp',
  tool: 'my-agent',
  model_version: 'v1',
  suggestions: [
    {
      tag: 'feature:dominant_orange',
      confidence: 0.71,
      reason: 'dominant orange pixels',
    },
  ],
}
```

The response is JSON on stdout. Errors are JSON on stderr.

Artist metadata from a sidecar library:

```sh
# preview
./target/debug/symtropy-agent artists --root /library/photos

# apply
./target/debug/symtropy-agent artists --root /library/photos --apply
```

This scans `.<stem>.halo.json` files, uses `sidecar.artists` as explicit Import
metadata, and treats matching flat `sidecar.tags` as reviewable suggestions.
Older sidecars often have an empty `artists[]` but store artist names in
`tag_parents` as `{ "child": "artist" }`; those children are used as artist
hints. `creator` parents work too.
It writes to the same `symtropy_tag_store.json5` file the UI uses. Close the UI
before applying a batch, or apply a fresh proposal if the store revision moved.

Read-only preservation report:

```sh
./target/debug/symtropy-agent preservation --root /library/photos
```

This lists sidecar tags missing from or hidden in the active tag index, along
with the sidecar `version`, store status, confidence, provenance, and any
`superseded_by` target. Explicit `preserved_tags` are reported as `preserved`:
they remain in the file but are withheld from the active gallery index. It never
edits sidecars or the tag store. Pass
`--include-accepted` to include active tags too. This is the headless counterpart
of the UI preservation view: the sidecar archive remains the preservation layer,
while the tag store only holds index state.

## Rust API

```rust
use symtropy_agent::{AgentBridge, AgentRequest};

let mut bridge = AgentBridge::load_default()?;
let response = bridge.handle(AgentRequest::Status)?;
```

See `src/lib.rs` for the full request/response vocabulary.

## Design rule

This bridge is intentionally narrow: it exposes the existing proposal pipeline,
not a second tag system. An agent may propose; the review UI may accept.
