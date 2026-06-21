#!/usr/bin/env bash
# Build a static Windows (mingw, x86_64) libopus for local cross-compiles.
#
# Why: cross-compiling the client to `x86_64-pc-windows-gnu` needs a
# Windows libopus to link, but `audiopus_sys`'s build-from-source path
# can't produce one on a macOS host. This script cross-builds libopus.a
# from the Opus source vendored inside `audiopus_sys`, into
# `vendor/opus-mingw/_install/lib/`, where build-windows-cross.sh expects
# it. Run once (re-run only after a toolchain or audiopus_sys bump).
#
# Requires (install once):
#   brew install mingw-w64 automake autoconf libtool pkg-config
#
# Output: vendor/opus-mingw/_install/lib/libopus.a (Intel amd64 COFF).
# The vendor/ build tree is gitignored.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_DIR="${REPO_ROOT}/vendor/opus-mingw"
HOST="x86_64-w64-mingw32"

if ! command -v "${HOST}-gcc" >/dev/null 2>&1; then
  echo "error: ${HOST}-gcc not found — run: brew install mingw-w64" >&2
  exit 1
fi

# Locate the Opus source vendored in the audiopus_sys crate. The path is
# .../registry/src/<index>/audiopus_sys-<ver>/opus, so search 3 levels.
OPUS_SRC="$(find "${HOME}/.cargo/registry/src" -maxdepth 3 -type d \
  -path '*audiopus_sys-*/opus' 2>/dev/null | sort | tail -1)"
if [ -z "${OPUS_SRC}" ] || [ ! -f "${OPUS_SRC}/autogen.sh" ]; then
  echo "error: could not find vendored Opus source under audiopus_sys." >&2
  echo "       run a normal 'cargo fetch' first so the crate is unpacked." >&2
  exit 1
fi

echo "Opus source : ${OPUS_SRC}"
echo "Build dir   : ${BUILD_DIR}"

mkdir -p "${BUILD_DIR}"
cp -r "${OPUS_SRC}/." "${BUILD_DIR}/"
cd "${BUILD_DIR}"

sh autogen.sh
./configure \
  --host="${HOST}" \
  --enable-static --disable-shared \
  --disable-doc --disable-extra-programs --with-pic \
  --prefix="${BUILD_DIR}/_install"
make -j"$(sysctl -n hw.ncpu 2>/dev/null || nproc)"
make install

echo
LIB="${BUILD_DIR}/_install/lib/libopus.a"
echo "Built: ${LIB}"
if "${HOST}-nm" "${LIB}" 2>/dev/null | grep -q ' T opus_decode$'; then
  echo "OK — defines opus_decode"
else
  echo "error: libopus.a missing opus_decode" >&2
  exit 1
fi
