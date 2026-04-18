#!/usr/bin/make -f
#
# Publish-toolkit integration for the Rust kloak crate. Mirrors the pattern
# used by sibling projects (metabrush, multitor, pick-a-boo): pure-Rust
# cross-compile via `rustup target add` + `cargo build --target`. No sysroot,
# no cross-gcc, no pkg-config plumbing — the Rust toolchain handles
# cross-compilation natively because kloak's Cargo.toml has no C-linking
# dependencies (libinput was replaced with a hand-rolled evdev reader).

CARGO  ?= cargo
BINARY := kloak
DEB_DIR := $(abspath $(CURDIR)/../deb)

.PHONY: all build release fmt fmt-check lint test check clean x86_64 aarch64 bootstrap

all : build

build :
	$(CARGO) build

release :
	$(CARGO) build --release

# Lint / format / unit-test surface for CI and local dev.
fmt :
	$(CARGO) fmt

fmt-check :
	$(CARGO) fmt --check

lint :
	$(CARGO) clippy --all-targets -- -D warnings

test :
	$(CARGO) test

check : fmt-check lint test

clean :
	$(CARGO) clean

# Publish-toolkit arch targets — stage the release binary under deb/<arch>/.
x86_64 :
	rustup target add x86_64-unknown-linux-gnu
	CARGO_TARGET_DIR=target/x86_64 \
	  $(CARGO) build --release --target x86_64-unknown-linux-gnu
	mkdir -p $(DEB_DIR)/amd64
	install -m 0755 target/x86_64/x86_64-unknown-linux-gnu/release/$(BINARY) \
	    $(DEB_DIR)/amd64/$(BINARY)

aarch64 :
	rustup target add aarch64-unknown-linux-gnu
	CARGO_TARGET_DIR=target/aarch64 \
	  $(CARGO) build --release --target aarch64-unknown-linux-gnu
	mkdir -p $(DEB_DIR)/arm64
	install -m 0755 target/aarch64/aarch64-unknown-linux-gnu/release/$(BINARY) \
	    $(DEB_DIR)/arm64/$(BINARY)

# Self-contained bootstrap: install Rust targets plus the arm64 linker.
# The linker is configured in rust/.cargo/config.toml — it's the only
# host-side tool we need beyond rustup now that kloak has no C-library deps.
bootstrap :
	sudo apt-get install -y gcc-aarch64-linux-gnu
	rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
