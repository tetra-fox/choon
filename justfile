default:
    @just --list

# cargo nextest with the default features, then the doc examples nextest does not run
test:
    cargo nextest run
    cargo test --doc

# formatting, clippy on the library and program with their tests, on the library alone without
# the program, with `serde` and with `testing`, and the docs as docs.rs builds them
lint:
    cargo fmt --all --check
    cargo clippy --all-targets -- -D warnings
    cargo clippy --no-default-features -- -D warnings
    cargo clippy --no-default-features --features serde -- -D warnings
    cargo clippy --no-default-features --features testing -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --no-default-features --features serde

# the library alone and the program on the manifest's rust-version
msrv:
    CARGO_TARGET_DIR=target/msrv nix develop .#msrv -c cargo build --locked --no-default-features
    CARGO_TARGET_DIR=target/msrv nix develop .#msrv -c cargo build --locked

# the gate for every push; ci.yml runs it
ci: lint test msrv

# package and build the crate as crates.io would, without uploading; release.yml runs it
publish-check:
    cargo publish --dry-run
