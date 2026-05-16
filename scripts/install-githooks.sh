#!/usr/bin/env bash
# One-time setup: point this clone's git hooks at scripts/githooks/ so
# pre-push runs the CI gate locally. Idempotent.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

git config core.hooksPath scripts/githooks
chmod +x scripts/githooks/* scripts/check.sh

echo "git hooks installed: core.hooksPath = scripts/githooks"
echo "test it with: git push --dry-run"
