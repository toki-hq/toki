# Toki app icon — build artifacts

This folder holds both the **source assets** (master SVG + hand-tuned
PNGs) and the **packaging artifacts** (`Toki.icns`, `Toki.ico`) that
the platform installers consume.

See [`icon-spec.md`](icon-spec.md) for the design rationale and the
"don't change" rules.

## Files

| File | Purpose |
|---|---|
| `toki-icon.svg` | Master source. Re-export the PNGs from this. |
| `toki-icon-{16,32,48,64,128,256,512,1024}.png` | Hand-tuned (or, for 48 px, downscaled) raster exports. |
| `Toki.icns` | macOS app bundle icon. Built from the PNG set via `iconutil`. |
| `Toki.ico` | Windows .exe / installer icon. Multi-size container with all six sizes embedded verbatim. |
| `build-platform-icons.sh` | Regenerate `Toki.icns` and `Toki.ico` from the PNGs. Idempotent. |

The PNGs and `.svg` are tracked. `Toki.icns` and `Toki.ico` are tracked
as build outputs so consumers of a fresh checkout don't need
`iconutil` / Pillow installed just to run the packagers.

## How the icon reaches the user

1. **Window chrome** (runtime). `crates/client/src/main.rs` embeds
   `toki-icon-256.png` via `include_bytes!` and hands it to
   `egui::IconData` → `ViewportBuilder::with_icon`. That covers macOS
   dock, Windows taskbar, Linux task switcher, and the OS titlebar.

2. **macOS `.app` bundle**. Packagers (e.g. `cargo bundle`,
   `create-dmg`, hand-crafted `.app`) should copy `Toki.icns` into the
   bundle's `Contents/Resources/` and set
   `CFBundleIconFile = Toki` in `Info.plist`.

3. **Windows `.exe` / installer**. Reference `Toki.ico` from
   `winres` / `embed-resource` / `cargo-wix` / NSIS / Inno Setup.
   Bundles intended for the Microsoft Store also accept the raw PNGs.

4. **Linux `.desktop`**. Install the PNGs into
   `/usr/share/icons/hicolor/<size>x<size>/apps/toki.png` and the
   master SVG at `/usr/share/icons/hicolor/scalable/apps/toki.svg`.
   Reference `Icon=toki` from the `.desktop` entry.

## Regenerating the platform artifacts

```bash
cd crates/client/assets/icon
./build-platform-icons.sh
```

The script reuses `iconutil` (built into macOS) for `.icns` and a
short Python+Pillow script for `.ico`. Run on macOS; for Linux/Windows
build hosts, run the script once on any Mac and commit the output —
the resulting files are platform-agnostic and reproducibly identical
from the same PNG inputs.

## Regenerating individual PNGs

Don't scale the existing PNGs. Re-render from `toki-icon.svg`:

```bash
rsvg-convert -w 64 -h 64 toki-icon.svg > toki-icon-64.png
# or
inkscape toki-icon.svg --export-type=png --export-filename=toki-icon-64.png -w 64 -h 64
```

Each size is independently tuned at design time (small sizes drop the
scanline overlay and ring glow); ad-hoc rescaling loses that work.
