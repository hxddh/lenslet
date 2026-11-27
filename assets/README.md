# Bundled assets

## The app icon

Lenslet's mark is a call and the call it caused (the white node, the cyan
one), joined into an L and seen through a lens: what the app does, and its
initial.

- `app-icon.svg`: the source, on its tile (1024 × 1024, the tile 824 × 824
  with the margin macOS icons keep).
- `app-icon.png`: the same at 1024 × 1024; `scripts/package-macos.sh`
  makes `AppIcon.icns` from it.
- `app-icon-512.png`: 512 × 512, and `app-icon.svg` as the scalable icon,
  for Linux (`scripts/package-linux.sh`).

The PNGs are rendered from the SVG, with transparent corners. Inside the
window the mark is drawn without its tile (`mark` in
`crates/app/src/ui/style.rs`), its L in the label color.

The window's other icons are line icons in `crates/app/src/ui/style.rs`.
