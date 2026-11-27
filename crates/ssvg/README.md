# SuperSVG (`.ssvg`)

SuperSVG is a **composite image format**: a portable, renderable description of
a layered raster composition plus baked previews. It is not an editor session
dump.

A package is a single ZIP file with this canonical layout:

```text
mimetype          application/x-symtropy-ssvg, first entry, stored uncompressed
manifest.json5    canvas, asset inventory, baked image paths
scene.json5       layer tree, variants, optional timeline
assets/*.webp     immutable source layers
preview.webp      small baked thumbnail
merged.webp       baked full-resolution composite
history/*         optional, non-normative; readers may ignore or strip it
```

`manifest.json5` and `scene.json5` are the normative documents. JSON5 is the
canonical encoding; `Package::to_json_bundle()` provides optional strict JSON
for tools that cannot read JSON5. Hand-written JSON5 may contain comments and
trailing commas.

## Scene model

- Sibling nodes are stored bottom-to-top. Groups contain children in the same
  order, and each node also carries an explicit `order` from the source stack.
- Every node has an id, name, `visible`, `opacity` (0.0..=1.0), `order`, blend
  mode, transform, and optional crop/frame. `visible: false` means disabled.
- `alphaInherit` records Krita-style alpha inheritance; `passThrough` records
  group compositing.
- An optional `source` object carries non-normative authoring provenance (Krita
  uuid, original filename, layer position, channel flags, locked/colour label
  and timeline flags). Renderers must ignore it.
- `params` are renderer-defined key/value overrides.
- `variants` are named parameter sets.
- `timeline.tracks[].keyframes[]` are parameter overrides at a point in time;
  no frame-by-frame snapshots are stored.

Blend modes cover the exported layers in the example projects, including
`multiply`, `screen`, `overlay`, `burn`, `dodge`, and `luminosity`.

## Krita converters

There are two converters. The **direct** reader opens a `.kra` on its own,
decodes Krita's 64x64 tile payloads (LZF or raw, BGRA channel planes), honours
`.defaultpixel`, and rebuilds the full layer tree. Use it when not every layer
was exported separately:

```sh
cargo run -p ssvg --example kra-direct-to-ssvg -- \
  'Example project/CatPlush/CatPlush.kra' \
  'Example project/CatPlush/CatPlush.ssvg'
```

The direct reader currently supports RGBA/8-bit (`PIXELSIZE 4`) KRA layers;
other colour depths are rejected with an explicit error instead of being
silently mis-decoded.

The export-based converter turns a `.kra` project plus an exported per-layer
WebP tree into one `.ssvg` package. It reads the KRA for canvas dimensions,
colour profile, baked preview/merged images and layer metadata, and reads the
WebP tree for the layer stack, group nesting, frame rectangles and filename
flags. Source files are never modified.

```sh
cargo run -p ssvg --example kra-to-ssvg -- \
  'Example project/CatPlush/CatPlush.kra' \
  'Example project/CatPlush' \
  'Example project/CatPlush/CatPlush.ssvg'
```

Krita export names use `Name#index(flags).webp`. Recognised flags are
`disabled`, `alpha`, blend modes such as `burn`, `overlay` and `luminosity`, and
opacity percentages such as `61%`. Directories may carry the same `#index`
suffix and become groups. This filename namespace is temporary input metadata
only: the emitted package uses clean paths such as
`assets/05-dress-coloring/10-dress-shading.webp`, records the index as `order`,
and stores visibility, opacity and blend mode directly in `scene.json5`.
One-frame WebP `ANMF` rectangles become `crop` plus `offset` so the renderer
places each cropped layer back on the original canvas.

The library API is also available as `ssvg::convert_kra_direct`,
`ssvg::build_package_direct`, `ssvg::convert_kra`,
`ssvg::convert_kra_with_options` and `ssvg::build_package`.

## What is deliberately absent

- undo stacks and layer edit history
- UI state or editor camera
- drawing-process timelapse
- vector editing commands

Optional `history/` entries are opaque and may be discarded by any renderer.

## Library use

```rust,no_run
use std::io::Cursor;
use ssvg::{Manifest, Node, Package, Scene};

# fn main() -> Result<(), String> {
let manifest = Manifest::default();
let scene = Scene {
    layers: vec![Node::layer("bg", "Background", "assets/bg.webp")],
    ..Scene::default()
};
let mut package = Package::new(manifest, scene, preview_webp(), merged_webp());
package.add_asset("assets/bg.webp", bg_webp());

let mut bytes = Vec::new();
package.write_to(Cursor::new(&mut bytes))?;
let reopened = ssvg::read_from_bytes(&bytes)?;
# let _ = reopened;
# Ok(())
# }

# fn preview_webp() -> Vec<u8> { Vec::new() }
# fn merged_webp() -> Vec<u8> { Vec::new() }
# fn bg_webp() -> Vec<u8> { Vec::new() }
```

The example above elides real WebP bytes; `Package::validate()` requires valid
WebP payloads before writing.
