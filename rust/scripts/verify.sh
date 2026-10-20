#!/usr/bin/env bash
# Isolated Rust verification entry.
#
# Usage:
#   rust/scripts/verify.sh                 # workspace test + debug/release build
#   rust/scripts/verify.sh --test <name>   # extra args go to cargo test
#
# Uses the existing Cargo installation. Optional:
#   PIHUB_RUST_TOOLCHAIN  explicit toolchain bin directory
#   CARGO_HOME            existing Cargo home
#   CARGO_TARGET_DIR      explicit target directory (recommended; keep it local)
#
# The integration test creates its own temporary PIHUB_HOME under /tmp. This
# script never reads or writes the production 2.0 deployment and never changes
# PATH/profile globally.
set -euo pipefail
umask 022

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUST_ROOT="$REPO_ROOT/rust"
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
  PIHUB_RUST_TOOLCHAIN=/path/to/toolchain/bin rust/scripts/verify.sh
No PATH/profile changes are made by this script.
EOF
  exit 1
fi

export CARGO_HOME="${CARGO_HOME:-$TMP_BASE/pihub-cargo-home}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$TMP_BASE/pihub-rust-target}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export PIHUB_APP_ROOT="$REPO_ROOT"

cd "$RUST_ROOT"
cargo test --workspace --locked "$@"
cargo build --locked --bin pihub
cargo build --locked --release --bin pihub

printf 'debug binary:   %s\n' "$CARGO_TARGET_DIR/debug/pihub"
printf 'release binary: %s\n' "$CARGO_TARGET_DIR/release/pihub"

# Launcher contract: native binary execution and an explicit no-fallback error.
VERSION="$(PIHUB_RUST_BIN="$CARGO_TARGET_DIR/release/pihub" "$REPO_ROOT/bin/pihub" --version)"
if [[ "$VERSION" != "3.0.0.0" ]]; then
  echo "launcher returned unexpected version: $VERSION" >&2
  exit 1
fi
set +e
LAUNCHER_ERROR="$(PIHUB_RUST_BIN="$TMP_BASE/pihub-rust-missing-binary" "$REPO_ROOT/bin/pihub" --version 2>&1 >/dev/null)"
LAUNCHER_STATUS=$?
set -e
if [[ $LAUNCHER_STATUS -eq 0 || "$LAUNCHER_ERROR" != *"native binary was not found"* ]]; then
  echo "launcher must fail closed without a native binary" >&2
  exit 1
fi
printf 'launcher ok: native 3.0 entry + explicit no-fallback error\n'
