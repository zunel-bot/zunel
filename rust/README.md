# rust/

Cargo workspace for the `zunel` binary and its supporting crates.

Build: `cargo build --workspace`
Test: `cargo test --workspace`
Lint: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

## Local CI gate

`scripts/check.sh` (at the repo root) mirrors the GitHub Actions gate so
`cargo fmt --check` / clippy / test regressions are caught before a push:

```bash
scripts/check.sh   # fmt --check, clippy -D warnings, test --workspace
```

To run the same gate automatically on `git push`, install the
repo-managed pre-push hook once per clone:

```bash
scripts/install-githooks.sh
```

This sets `core.hooksPath` to `scripts/githooks/` for this clone only;
nothing global is touched. Bypass with `git push --no-verify` if needed.
