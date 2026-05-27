#!/usr/bin/env bash
# Run the workspace test suite under cargo-llvm-cov and emit:
#
#   * target/coverage/html/index.html — human-readable report
#   * target/coverage/lcov.info       — machine-readable for CI
#
# Requires (install once):
#   cargo install cargo-llvm-cov
#   rustup component add llvm-tools-preview
#
# Re-run any time after changing tests or code. Outputs are
# gitignored so they don't pollute the working tree.
#
# Flags:
#   --html-only   skip the lcov output, just regenerate the HTML
#   --ci          fail on coverage drop or missing files (intended
#                 for invocation from CI; otherwise lenient)

set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    echo "error: cargo-llvm-cov not installed."
    echo "       cargo install cargo-llvm-cov"
    echo "       rustup component add llvm-tools-preview"
    exit 1
fi

mkdir -p target/coverage

HTML_ONLY=0
CI=0
for arg in "$@"; do
    case "$arg" in
        --html-only) HTML_ONLY=1 ;;
        --ci) CI=1 ;;
        -h|--help)
            sed -n '2,/^set/p' "$0" | sed 's/^# \{0,1\}//;/^set/d'
            exit 0
            ;;
        *) echo "unknown flag: $arg" >&2; exit 2 ;;
    esac
done

# `--ignore-filename-regex` skips generated proto stubs and test
# files themselves — they'd otherwise inflate the denominator
# without saying anything meaningful about what we wrote.
COMMON_FLAGS=(
    --workspace
    --ignore-filename-regex
    '(.*toki\.v1\.rs|.*/tests/.*)'
)

echo "→ running tests under cargo-llvm-cov"
cargo llvm-cov clean --workspace

if [[ "$HTML_ONLY" -eq 0 ]]; then
    # lcov first; --no-clean lets the second invocation reuse the
    # same instrumentation pass for the HTML report.
    cargo llvm-cov "${COMMON_FLAGS[@]}" \
        --lcov \
        --output-path target/coverage/lcov.info
    echo "   wrote target/coverage/lcov.info"
fi

cargo llvm-cov "${COMMON_FLAGS[@]}" \
    --no-clean \
    --html \
    --output-dir target/coverage
echo "   wrote target/coverage/html/index.html"

# Summary line for the terminal — same metric the HTML page tops
# out with. CI consumers should parse lcov.info instead. The
# `report` subcommand doesn't accept --workspace, only the test-
# running invocations above do.
echo
cargo llvm-cov report --summary-only

if [[ "$CI" -eq 1 && ! -s target/coverage/lcov.info ]]; then
    echo "error: lcov.info missing or empty" >&2
    exit 1
fi
