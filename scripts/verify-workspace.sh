#!/usr/bin/env bash
# Kova workspace quality gate (KOVA-003). Run before opening any PR.
set -euo pipefail
cd "$(dirname "$0")/.."

TOTAL=7
STEP=0
START=$(date +%s)
CURRENT=""

on_exit() {
  status=$?
  if [ "$status" -ne 0 ] && [ "$status" -ne 2 ]; then
    echo "FAILED at step [$STEP/$TOTAL] $CURRENT" >&2
  fi
}
trap on_exit EXIT

step() {
  STEP=$((STEP + 1))
  CURRENT="$1"
  shift
  echo "==> [$STEP/$TOTAL] $CURRENT"
  local t0
  t0=$(date +%s)
  "$@"
  echo "    ok ($(( $(date +%s) - t0 ))s)"
}

require_toolchain() {
  for tool in cargo rustc; do
    command -v "$tool" >/dev/null 2>&1 || {
      echo "error: '$tool' not found. Install Rust via rustup (https://rustup.rs)." >&2
      exit 2
    }
  done
  cargo --version >/dev/null 2>&1 || {
    echo "error: 'cargo' failed to run. Configure a Rust toolchain via rustup." >&2
    exit 2
  }
  rustc --version >/dev/null 2>&1 || {
    echo "error: 'rustc' failed to run. Configure a Rust toolchain via rustup." >&2
    exit 2
  }
  cargo clippy --version >/dev/null 2>&1 || {
    echo "error: 'cargo clippy' component not found. Install via 'rustup component add clippy'." >&2
    exit 2
  }
  cargo fmt --version >/dev/null 2>&1 || {
    echo "error: 'cargo fmt' component not found. Install via 'rustup component add rustfmt'." >&2
    exit 2
  }
  cargo --version
  rustc --version
  cargo clippy --version
  cargo fmt --version
}

step "Toolchain"          require_toolchain
step "Format"             cargo fmt --all -- --check
step "Clippy"             cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
step "Tests (workspace)"  cargo test --workspace --all-features --locked
step "Headless guardrail" cargo test -p kova_core --test headless_guardrail --locked
step "Headless w/o UI"    env -u DISPLAY -u WAYLAND_DISPLAY \
                          cargo test --workspace --exclude kova_app --exclude kova_ui --all-features --locked
step "Rustdoc"            env RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

echo "OK — workspace verified in $(( $(date +%s) - START ))s"
