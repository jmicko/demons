PREFIX ?= $(HOME)/.cargo

.PHONY: build check install test

build:
	cargo build --release --locked

check:
	cargo fmt -- --check
	cargo clippy --all-targets --all-features -- -D warnings

test: check
	cargo test --all-targets --all-features --locked

install:
	CARGO_INSTALL_ROOT="$(PREFIX)" cargo install --path . --locked --force
