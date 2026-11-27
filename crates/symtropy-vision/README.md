# symtropy-vision

Local-only image analysis for reviewable tag suggestions.

This crate never contacts a network and never downloads a model file or runtime
binary. The default backend derives simple feature tags from local pixels:
dominant hue, tone, and aspect ratio. It also reads the baked `preview.webp`
inside `.ssvg` files, so no live scene rendering is needed.

## Explicit ONNX backend

Build with `--features onnx` (or `symtropy-ui --features onnx-vision`) and provide
local paths for:

- the ONNX Runtime shared library, e.g. `libonnxruntime.so.1`
- the `.onnx` model file
- a labels text file

The labels file contains one canonical tag per line. An optional display name may
follow `|`:

```text
# comments are ignored
act:possession | Claimed possession
state:possessed
theme:ownership
```

The UI stores these paths in local state. Nothing is fetched automatically. The
model input is square (`input_size`, 224 by default) and supports NCHW or NHWC
float tensors with raw, 0–1, or CLIP normalization. The first output is read as
logits by default; it can also be treated as probabilities. Scores are ranked,
filtered by minimum confidence, and truncated to `top_k`.

Every emission is a `LocalSuggestion` with `Suggested` state and a model version.
The review store is responsible for staging it; accepted or locked human claims
are never overwritten.
