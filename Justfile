set positional-arguments

check:
    cargo fmt --check
    cargo-clippy clippy --all-targets --all-features -- -D warnings -W clippy::pedantic -W clippy::nursery

run:
    cargo run
