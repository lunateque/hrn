set shell := ["sh", "-eu", "-c"]
set windows-shell := ["sh", "-eu", "-c"]

ci:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo xtask check
    cargo test --workspace --all-features
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

removability:
    cargo xtask removability

fix:
    cargo fmt --all
    cargo clippy --fix --allow-dirty --allow-staged --workspace --all-targets --all-features
