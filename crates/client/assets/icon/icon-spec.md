# App Icon — Toki

The final icon is **Concept C · Speaker Grille**, locked in v1.0.

A phosphor-lit hex-packed dot field on a matte-black squircle, surrounded
by a faint outer speaker ring. Three brightness bands plus a single
ultra-bright center anchor make the silhouette legible at every size from
1024 × 1024 down to 16 × 16.

The icon shares the **chassis gradient** (`#1a1c1e → #0a0b0a → #040504`) and
the **phosphor accent** (`oklch(0.86 0.18 145) ≈ #7FFF90`) with the desktop
widget so the app and its icon read as one piece.

---

## Files in this folder

| File | Format | Size | Use |
|---|---|---|---|
| `toki-icon.svg` | SVG | vector | Master source for re-export |
| `toki-icon-1024.png` | PNG | 1024 × 1024 | App Store / stores |
| `toki-icon-512.png` | PNG | 512 × 512 | macOS @2x retina |
| `toki-icon-256.png` | PNG | 256 × 256 | macOS Finder grid |
| `toki-icon-128.png` | PNG | 128 × 128 | macOS dock |
| `toki-icon-64.png` | PNG | 64 × 64 | Windows / Linux launcher |
| `toki-icon-32.png` | PNG | 32 × 32 | system tray |
| `toki-icon-16.png` | PNG | 16 × 16 | favicon / tab bar |

The PNGs are pre-rendered (not SVG-to-PNG at runtime). Each one is hand-tuned
for its size — small sizes drop the scanline overlay and ring glow so the
remaining pixels read clearly.

---

## How to integrate

### macOS (`.icns`)

Build an `.iconset` folder with Apple's naming convention, then call
`iconutil`:

```bash
mkdir Toki.iconset
cp toki-icon-16.png   Toki.iconset/icon_16x16.png
cp toki-icon-32.png   Toki.iconset/icon_16x16@2x.png
cp toki-icon-32.png   Toki.iconset/icon_32x32.png
cp toki-icon-64.png   Toki.iconset/icon_32x32@2x.png
cp toki-icon-128.png  Toki.iconset/icon_128x128.png
cp toki-icon-256.png  Toki.iconset/icon_128x128@2x.png
cp toki-icon-256.png  Toki.iconset/icon_256x256.png
cp toki-icon-512.png  Toki.iconset/icon_256x256@2x.png
cp toki-icon-512.png  Toki.iconset/icon_512x512.png
cp toki-icon-1024.png Toki.iconset/icon_512x512@2x.png

iconutil -c icns Toki.iconset
```

Drop the resulting `Toki.icns` into the app bundle's `Contents/Resources/`
and reference it from `Info.plist`:

```xml
<key>CFBundleIconFile</key>
<string>Toki.icns</string>
```

### Windows (`.ico`)

ImageMagick or any `.ico` packer accepting multiple PNG inputs:

```bash
convert toki-icon-16.png toki-icon-32.png toki-icon-48.png \
        toki-icon-64.png toki-icon-128.png toki-icon-256.png \
        Toki.ico
```

(Render a 48 px frame from the SVG if you don't have one pre-rendered —
Windows still uses 48 px in some legacy contexts.)

For Rust with `tauri`: set `tauri.conf.json` → `tauri.bundle.icon` to the
PNG list. For `iced` or `egui`: load the PNG with `image::open()` and pass
to `WindowAttributes::with_window_icon()` (winit).

### Linux

Drop the PNGs in `hicolor/<size>x<size>/apps/toki.png`:

```
/usr/share/icons/hicolor/16x16/apps/toki.png
/usr/share/icons/hicolor/32x32/apps/toki.png
/usr/share/icons/hicolor/64x64/apps/toki.png
/usr/share/icons/hicolor/128x128/apps/toki.png
/usr/share/icons/hicolor/256x256/apps/toki.png
/usr/share/icons/hicolor/512x512/apps/toki.png
```

Plus a `.desktop` entry referencing `Icon=toki`. Also include the SVG at
`hicolor/scalable/apps/toki.svg` so high-DPI displays can scale.

### Rust framework specifics

| Framework | How to set the window icon |
|---|---|
| `eframe` / `egui` | `eframe::NativeOptions::viewport.with_icon(egui::IconData { … })` — load PNG bytes via `image::load_from_memory_with_format` |
| `iced` | `iced::window::Settings::icon(icon)` — `icon::from_file_data(bytes, None)` |
| `Slint` | `Window { icon: @image-url("toki-icon-256.png"); }` |
| `tauri` | `tauri.conf.json` → `bundle.icon: ["toki-icon-32.png", "toki-icon-128.png", "toki-icon-256.png", "Toki.icns", "Toki.ico"]` |

For the tray icon (used when "minimize to tray" is enabled, see
`behavior-spec.md`), prefer the **32 px** PNG on Windows/Linux and the
**16 px** on macOS (macOS tray icons are small and monochrome-leaning;
the white center dot in `toki-icon-16.png` is what survives).

---

## Re-rendering from the master

The SVG is the source of truth. If you ever need a size that's not in
the bundled set:

```bash
# Inkscape
inkscape toki-icon.svg --export-type=png --export-filename=toki-icon-48.png -w 48 -h 48

# rsvg-convert (faster, available via librsvg)
rsvg-convert -w 48 -h 48 toki-icon.svg > toki-icon-48.png

# Or use the prototype's render script (see /scrap/ in the design project)
```

**Don't** scale the existing PNGs up or down — each one is hand-tuned for
its size. Re-render from SVG instead, or accept slight quality loss.

---

## Don't change

These are non-negotiable for brand continuity with the desktop widget:

- **Primary color**: `oklch(0.86 0.18 145)` (`#7FFF90`). Do not shift the
  hue or saturation.
- **Chassis gradient**: top `#1a1c1e` → mid `#0a0b0a` → bottom `#040504`.
- **Corner radius**: 22% of the icon's edge (matches macOS squircle).
- **Center anchor dot**: white core with bright phosphor halo. This is
  what makes the icon legible at 16 px — don't remove it.

## Variants OK to add

- **Tactical / Cyber / Stealth** color variants (recolor primary only,
  mirror the theme swatches from `design-tokens.md`). Use these for
  light/dark mode-aware system trays or for special builds.
- **Monochrome** version for macOS template images: replace the green
  with pure white at 100% opacity; remove the bloom glow.
- **Notification badge overlay**: stack a red `WifiOff` glyph at the
  bottom-right corner when the radio is offline (matches `OfflineCenter`
  iconography from `components.md`).
