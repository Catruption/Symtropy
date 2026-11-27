# Icon assets

`assets/icons/` is the canonical runtime icon set. `build.rs` indexes every
decodable image here by filename stem, so the Rust code never hardcodes icon
names or formats.

SVG and KRA files are authoring sources only. The app does not decode them at
runtime. Re-bake edited SVGs into lossless WebP with:

```bash
./regenerate.sh
```

The script renders every SVG at 512×512 and writes the matching `.webp` file.
Set `SYMTROPY_ICON_SIZE` to change the raster size. Keep theme overrides in
`assets/themes/<theme>/icons/` with the same stem; a theme file wins over the
generic one.

Existing `.png` icons without an SVG source are left as-is because upscaling a
bitmap cannot recover vector detail.
