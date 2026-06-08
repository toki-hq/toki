#!/usr/bin/env bash
# Cross-compile the Toki client to a Windows .exe from macOS/Linux.
#
# Why this script exists:
#   `audiopus_sys` (the Opus codec) links a prebuilt libopus on the
#   `windows-msvc` target, but on `windows-gnu` (mingw) it tries to BUILD
#   Opus from source with the host toolchain — which fails when the host
#   is macOS (wrong arch / no Windows libopus). The fix is to point it at
#   a mingw-built static libopus via OPUS_LIB_DIR + OPUS_STATIC, and skip
#   pkg-config with OPUS_NO_PKG.
#
#   These env vars MUST NOT leak into a native (macOS/Linux) build, where
#   they'd make audiopus_sys link the *Windows* lib. cargo's [env] table
#   is global and [target.*] can't set env, so scoping lives here instead
#   of in .cargo/config.toml.
#
# Prerequisites (macOS):
#   brew install mingw-w64 automake autoconf libtool pkg-config
#   rustup target add x86_64-pc-windows-gnu
#   Then build the mingw libopus once: scripts/build-opus-mingw.sh
#
# Usage:
#   scripts/build-windows-cross.sh [--release] [extra cargo args...]
#
# NOTE: The official release .exe is built by CI on a native windows-latest
# MSVC runner (see .github/workflows/ci.yml). This cross-build is for local
# testing only and produces a *GNU*-ABI binary, not the MSVC one shipped.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OPUS_LIB_DIR="${REPO_ROOT}/vendor/opus-mingw/_install/lib"

if [ ! -f "${OPUS_LIB_DIR}/libopus.a" ]; then
  echo "error: mingw libopus not found at ${OPUS_LIB_DIR}/libopus.a" >&2
  echo "       run scripts/build-opus-mingw.sh first." >&2
  exit 1
fi

# audiopus_sys does NOT declare `rerun-if-env-changed` for OPUS_LIB_DIR,
# so cargo happily reuses a stale build-script output from an earlier run
# that didn't have these vars set — producing the "undefined reference to
# opus_decode" link error even though the prebuilt lib exists. Force its
# build script to re-resolve by clearing its cached output for this target.
TARGET_DIR="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
for prof in debug release; do
  rm -rf "${TARGET_DIR}/x86_64-pc-windows-gnu/${prof}/build/"audiopus_sys-* 2>/dev/null || true
done

exec env \
  OPUS_NO_PKG=1 \
  OPUS_STATIC=1 \
  OPUS_LIB_DIR="${OPUS_LIB_DIR}" \
  cargo build -p toki-client --target x86_64-pc-windows-gnu "$@"
