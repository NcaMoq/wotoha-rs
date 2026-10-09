# Contributing to Wotoha RS

Wotoha RS is a Rust Discord music bot with an audio-analysis and AutoMix
research surface. Contributions are welcome when they keep playback behavior,
security boundaries, and reproducibility explicit.

## Start here

1. Fork or clone the repository and use the Rust toolchain pinned in
   [`rust-toolchain.toml`](rust-toolchain.toml).
2. Read [`docs/development.md`](docs/development.md) for host packages,
   container parity, and the analysis-lab boundary.
3. Keep credentials in untracked files. Never commit Discord tokens,
   provider cookies, private URLs, raw media, screenshots, or external raw
   observations.

## Before opening a pull request

Run the same quality gates used by CI:

```bash
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked --no-deps -- -D warnings
for script in deploy/*.sh deploy/tests/*.sh; do bash -n "$script"; done
bash deploy/tests/run.sh
```

If your change touches the research lab, also run the focused lab tests and
`cargo run --locked -p wotoha-analysis-lab -- --help`. Keep generated fixture
audio and reports outside the worktree.

## Scope and design expectations

- Keep production changes separate from research-only analysis-lab work.
- Preserve bounded candidate sets, deterministic behavior, and explicit
  fallback paths in AutoMix changes.
- Treat external observations as neutral evaluation evidence, never as a
  replacement for repository-native ground truth or independent tests.
- Keep media-provider behavior and deployment contracts compatible unless the
  pull request explains the migration and includes regression coverage.
- Update user-facing documentation when commands, configuration, deployment,
  or supported media behavior changes.

## Issues and pull requests

For playback issues, include the provider, URL type, version/commit, platform,
and sanitized logs. Reproduction steps should not include secrets or private
media. For AutoMix or analysis changes, describe the evidence, the fallback
behavior, and how you verified that unrelated playback paths did not change.

Small focused pull requests are easier to review. Explain any intentional
trade-off instead of presenting an unverified benchmark or an absolute claim.
