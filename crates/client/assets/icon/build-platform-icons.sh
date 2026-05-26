#!/usr/bin/env bash
# Regenerate Toki.icns (macOS) and Toki.ico (Windows) from the
# hand-tuned PNG set in this directory. Idempotent: rerun any time
# after updating the PNGs.
#
# Requirements:
#   * macOS  — for `iconutil`, used to pack the .icns.
#   * Python 3 + Pillow — used to pack the .ico (Pillow's built-in
#     ICO writer resamples a single source, so we write the file by
#     hand to embed each PNG verbatim per the icon-spec).
#
# Run from this directory (`crates/client/assets/icon`).

set -euo pipefail
cd "$(dirname "$0")"

if ! command -v iconutil >/dev/null 2>&1; then
    echo "error: iconutil not found — Toki.icns requires macOS." >&2
    exit 1
fi
if ! python3 -c "import PIL" >/dev/null 2>&1; then
    echo "error: Python Pillow not found — install with: pip3 install pillow" >&2
    exit 1
fi

# ── macOS Toki.icns ───────────────────────────────────────────────
echo "→ building Toki.icns"
rm -rf Toki.iconset
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
rm -rf Toki.iconset

# ── Windows Toki.ico ──────────────────────────────────────────────
echo "→ building Toki.ico"
python3 - <<'PY'
"""
Concatenate the hand-tuned PNGs into a single .ico. Embeds each PNG
verbatim (no resampling) — the icon-spec is explicit that the bundled
PNGs are individually tuned and shouldn't be rescaled.

ICO file layout (little-endian):
  ICONDIR (6 bytes): reserved=0 u16, type=1 u16, count u16
  ICONDIRENTRY × N (16 bytes each):
    width u8 (0 == 256), height u8 (0 == 256),
    colors u8 = 0, reserved u8 = 0,
    planes u16 = 1, bpp u16 = 32,
    size u32 (bytes of image blob),
    offset u32 (offset from file start to image blob)
  Image blobs in the same order.
"""
import struct, pathlib

sizes = [16, 32, 48, 64, 128, 256]
blobs = []
for s in sizes:
    p = pathlib.Path(f"toki-icon-{s}.png")
    if not p.exists():
        # 48 px isn't in the hand-tuned set; the icon-spec explicitly
        # allows downscaling from 64 for this one slot.
        from PIL import Image
        Image.open("toki-icon-64.png").resize((s, s), Image.LANCZOS).save(p)
        print(f"  synthesised {p.name}")
    blobs.append(p.read_bytes())

count = len(sizes)
header = struct.pack("<HHH", 0, 1, count)
dir_size = 6 + 16 * count
entries = bytearray()
offset = dir_size
for s, blob in zip(sizes, blobs):
    w = h = 0 if s >= 256 else s
    entries += struct.pack(
        "<BBBBHHII", w, h, 0, 0, 1, 32, len(blob), offset
    )
    offset += len(blob)

with open("Toki.ico", "wb") as f:
    f.write(header)
    f.write(entries)
    for blob in blobs:
        f.write(blob)
PY

echo "done."
ls -la Toki.icns Toki.ico
