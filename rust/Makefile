#!/usr/bin/make -f
#
# Publish-toolkit integration for the Rust kloak crate. Mirrors the targets
# of c/Makefile so the repo-wide publish workflow keeps working.

CARGO  ?= cargo
BINARY := kloak
DEB_DIR := $(abspath $(CURDIR)/../deb)

# The arm64 sysroot is produced by build-arm64-sysroot.sh — a self-contained
# fetch-from-ports.ubuntu.com script that drops libinput/libevdev/etc. under
# $(CURDIR)/sysroot-arm64/. No apt magic required.
SYSROOT_ARM64 := $(abspath $(CURDIR)/sysroot-arm64/root)

TARGET_DIR_X86_64  := target/x86_64-unknown-linux-gnu/release
TARGET_DIR_AARCH64 := target/aarch64-unknown-linux-gnu/release

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
	$(CARGO) build --release --target x86_64-unknown-linux-gnu
	mkdir -p $(DEB_DIR)/amd64
	install -m 0755 $(TARGET_DIR_X86_64)/$(BINARY) $(DEB_DIR)/amd64/$(BINARY)

# arm64 cross-build. Uses the self-contained sysroot at rust/sysroot-arm64/
# (fetched from ports.ubuntu.com by build-arm64-sysroot.sh). pkg-config is
# redirected into the sysroot so input-sys sees the arm64 libinput.pc, not
# the host amd64 one. The linker is the cross-gcc so crt objects and libc
# come from the sysroot glued together with the cross toolchain's libgcc.
aarch64 :
	@command -v aarch64-linux-gnu-gcc >/dev/null 2>&1 || { \
	  echo "ERROR: aarch64-linux-gnu-gcc not found. Run: make -C rust bootstrap"; \
	  exit 1; \
	}
	@rustup target list --installed 2>/dev/null | grep -q '^aarch64-unknown-linux-gnu$$' || { \
	  echo "ERROR: rust target aarch64-unknown-linux-gnu not installed."; \
	  echo "       Run: rustup target add aarch64-unknown-linux-gnu"; \
	  exit 1; \
	}
	@test -f $(SYSROOT_ARM64)/usr/lib/aarch64-linux-gnu/pkgconfig/libinput.pc || \
	    $(CURDIR)/build-arm64-sysroot.sh
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS='-C link-arg=--sysroot=$(SYSROOT_ARM64) -C link-arg=-Wl,--allow-shlib-undefined' \
	PKG_CONFIG_ALLOW_CROSS=1 \
	PKG_CONFIG_PATH= \
	PKG_CONFIG_LIBDIR=$(SYSROOT_ARM64)/usr/lib/aarch64-linux-gnu/pkgconfig:$(SYSROOT_ARM64)/usr/share/pkgconfig \
	PKG_CONFIG_SYSROOT_DIR=$(SYSROOT_ARM64) \
	$(CARGO) build --release --target aarch64-unknown-linux-gnu
	mkdir -p $(DEB_DIR)/arm64
	install -m 0755 $(TARGET_DIR_AARCH64)/$(BINARY) $(DEB_DIR)/arm64/$(BINARY)

# Self-contained bootstrap: install host-side cross toolchain and Rust target.
# Mirrors c/Makefile's bootstrap but adds the Rust toolchain bits.
bootstrap :
	sudo apt-get install -y \
	    libevdev-dev libinput-dev pkg-config \
	    crossbuild-essential-arm64
	rustup target add aarch64-unknown-linux-gnu
