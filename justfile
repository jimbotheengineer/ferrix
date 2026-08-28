# Ferrix task runner. `cargo install just` if you don't have it.

default:
    @just --list

build:
    cargo build --release

test:
    cargo test --workspace

fmt:
    cargo fmt --all

lint:
    cargo clippy --workspace --all-targets -- -D warnings

run FILE="":
    cargo run --release -p ferrix-ui -- {{FILE}}

# Generate data, benchmark it, and delete it — the default, so nothing is
# left on disk. Use `just bench-keep` if you need the file to stick around.
bench ROWS="10000000":
    mkdir -p benchdata
    cargo build --release -p ferrix-bench
    ./target/release/gen-data {{ROWS}} benchdata/bench.csv
    ./target/release/bench-load benchdata/bench.csv
    @just clean-data

bench-keep ROWS="10000000":
    mkdir -p benchdata
    cargo build --release -p ferrix-bench
    ./target/release/gen-data {{ROWS}} benchdata/bench.csv
    ./target/release/bench-load benchdata/bench.csv
    @echo "NOTE: benchdata/ kept on disk. Run 'just clean-data' when done."

# Remove generated benchmark data.
clean-data:
    rm -rf benchdata
    @echo "benchdata/ removed"

clean-all: clean-data
    cargo clean
