#!/bin/bash

## For CodeQL autobuild

set -x
set -e

sudo --non-interactive apt-get update --error-on=any
# kloak is now pure-Rust (no libinput, no libevdev, no pkg-config); apt-get
# invocation kept because CodeQL expects at least one system prep step.
sudo --non-interactive apt-get install --yes ca-certificates

# Rust toolchain: use rustup if available, otherwise fall back to the distro
# rustc (CodeQL images ship with rustup).
if ! command -v cargo >/dev/null 2>&1; then
    sudo --non-interactive apt-get install --yes rustc cargo
fi

make -C rust
