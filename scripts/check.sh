#!/usr/bin/env bash
# Mirror the Rust CI gate so `cargo fmt`/clippy regressions are caught
# before a push. Run from anywhere in the repo:
#
#   scripts/check.sh
#
# Used by scripts/githooks/pre-push (install via scripts/install-githooks.sh).

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root/rust"

echo "==> cargo fmt --check"
cargo fmt --all -- --check

echo "==> cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> cargo test --workspace"
cargo test --workspace --no-fail-fast

echo "OK: fmt + clippy + test all green"
