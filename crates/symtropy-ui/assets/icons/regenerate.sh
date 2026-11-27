#!/usr/bin/env bash
# Re-bake SVG icon sources into lossless WebP textures.
#
# The app deliberately does not decode SVG at runtime. These raster files are
# the canonical payloads discovered by build.rs, while the SVGs remain editable
# sources. 512px gives enough headroom for large gallery tiles on HiDPI screens.
set -euo pipefail

cd "$(dirname "$0")"
SIZE=${SYMTROPY_ICON_SIZE:-512}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

shopt -s nullglob
for svg in *.svg; do
    stem=${svg%.svg}
    inkscape "$svg" \
        --export-type=png \
        --export-area-page \
        --export-width="$SIZE" \
        --export-height="$SIZE" \
        --export-filename="$tmp/$stem.png" >/dev/null
    magick "$tmp/$stem.png" \
        -alpha on \
        -define webp:lossless=true \
        -define webp:method=6 \
        "$stem.webp"
done

echo "Re-baked SVG icons at ${SIZE}x${SIZE}."
