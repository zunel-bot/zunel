#!/usr/bin/env bash
# Mirror the Rust CI gate so `cargo fmt`/clippy regressions are caught
# before a push. Run from anywhere in the repo:
#
#   scripts/check.sh
#
# Used by scripts/githooks/pre-push (install via scripts/install-githooks.sh).
#
# Optional bare-HOME pass: `ZUNEL_CHECK_BARE_HOME=1 scripts/check.sh` runs
# the MCP self-server tests against an empty `$HOME` to catch the failure
# mode where a developer's onboarded config masks the no-config code path
# (the round-2..7 bug that ran red on CI for weeks). Off by default
# because the dev pre-push hook needs to stay fast.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root/rust"

echo "==> cargo fmt --check"
cargo fmt --all -- --check

echo "==> cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> cargo test --workspace"
cargo test --workspace --no-fail-fast

if [ "${ZUNEL_CHECK_BARE_HOME:-0}" = "1" ]; then
  echo "==> cargo test (bare HOME — catches no-onboard regressions)"
  bare_home="$(mktemp -d)"
  HOME="$bare_home" cargo test -p zunel-mcp-self --test server_test --test http_test --no-fail-fast
  HOME="$bare_home" cargo test -p zunel-cli --test mcp_cli_test --no-fail-fast
  rm -rf "$bare_home"
fi

echo "OK: fmt + clippy + test all green"
