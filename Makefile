PREFIX ?= $(HOME)/.cargo
RELEASE_TARGET_DIR ?= /tmp/demons-release-check-target

.PHONY: build check install test release-check

build:
	cargo build --release --locked

check:
	cargo fmt -- --check
	cargo clippy --all-targets --all-features -- -D warnings

test: check
	cargo test --all-targets --all-features --locked

release-check: test
	CARGO_TARGET_DIR="$(RELEASE_TARGET_DIR)" cargo package --locked --allow-dirty

install:
	CARGO_INSTALL_ROOT="$(PREFIX)" cargo install --path . --locked --force
