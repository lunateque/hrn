## Summary

<!-- Briefly describe what changed and why. -->

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo xtask check` (layer boundaries, `<= 600` LOC/file, module isolation)
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
