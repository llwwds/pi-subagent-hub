#!/usr/bin/env bash
# Build the native 3.0 binary with the existing Cargo installation.
#
# Optional environment:
#   CARGO_TARGET_DIR      default: ${TMPDIR:-/tmp}/pihub-rust-target
#   CARGO_HOME            existing Cargo home is respected
#   PIHUB_RUST_TOOLCHAIN  explicit toolchain bin directory (optional; no
#                         machine-specific path is committed)
#   RUSTC                 explicit rustc path (optional)
set -euo pipefail
umask 022

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TMP_BASE="${TMPDIR:-/tmp}"
TMP_BASE="${TMP_BASE%/}"

if [[ -n "${PIHUB_RUST_TOOLCHAIN:-}" ]]; then
  export PATH="$PIHUB_RUST_TOOLCHAIN:$PATH"
  if [[ -z "${RUSTC:-}" ]]; then
    export RUSTC="$PIHUB_RUST_TOOLCHAIN/rustc"
  fi
fi

if ! command -v cargo >/dev/null 2>&1; then
  cat >&2 <<'EOF'
cargo was not found on PATH.
Install Rust or point at an existing toolchain explicitly:
  PIHUB_RUST_TOOLCHAIN=/path/to/toolchain/bin bash rust/scripts/build-release.sh
No PATH/profile changes are made by this script.
EOF
  exit 1
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$TMP_BASE/pihub-rust-target}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"

cargo build --release --locked \
  --manifest-path "$REPO_ROOT/rust/Cargo.toml" \
  --bin pihub "$@"

printf 'release binary: %s\n' "$CARGO_TARGET_DIR/release/pihub"
