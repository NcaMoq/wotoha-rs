## What changed

<!-- Summarize the user-visible or maintenance change. -->

## Why

<!-- Explain the problem or decision this pull request addresses. -->

## Testing

<!-- List commands and focused checks. Include failures or skipped checks. -->

## Checklist

- [ ] Production behavior and research-only behavior remain clearly separated.
- [ ] Documentation, configuration, or release notes are updated when needed.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo test --workspace --locked` passes.
- [ ] `cargo clippy --workspace --all-targets --no-deps -- -D warnings` passes.
