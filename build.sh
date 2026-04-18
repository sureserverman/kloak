#!/bin/bash

## For CodeQL autobuild

set -x
set -e

sudo --non-interactive apt-get update --error-on=any
sudo --non-interactive apt-get install --yes libevdev2 libevdev-dev libinput10 libinput-dev pkg-config

# Rust toolchain: use rustup if available, otherwise fall back to the distro
# rustc (CodeQL images ship with rustup).
if ! command -v cargo >/dev/null 2>&1; then
    sudo --non-interactive apt-get install --yes rustc cargo
fi

make -C rust
