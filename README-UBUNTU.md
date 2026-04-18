# kloak for Ubuntu

Ubuntu `.deb` build of [Whonix/kloak](https://github.com/Whonix/kloak) — keystroke and mouse-movement anonymization via timing randomization. Wayland-only.

## Layout

Follows the project's standard deb convention:

```
kloak-ubuntu/
├── src/                         upstream C source
├── protocol/                    upstream Wayland protocol XML
├── Makefile                     upstream make rules (compiles kloak binary + man page)
├── etc/apparmor.d/              upstream AppArmor profiles (shipped as-is)
├── usr/                         upstream helper + systemd unit (shipped as-is)
├── deb/
│   ├── amd64/                   compiled binary staged here by build-deb.sh
│   └── package/
│       ├── DEBIAN/
│       │   ├── control
│       │   ├── postinst         reloads AppArmor profile, systemd daemon-reload
│       │   └── prerm            stops and disables the service
│       ├── etc/apparmor.d/      (populated at build time)
│       ├── usr/bin/kloak
│       ├── usr/libexec/kloak/
│       ├── usr/lib/systemd/system/kloak.service
│       └── usr/share/man/man8/kloak.8.gz
├── build-deb.sh                 one-shot build
├── debian/                      upstream Whonix debhelper tree (unused here; kept for diff reference)
└── README-UBUNTU.md
```

## Build

```
./build-deb.sh
```

Script compiles via upstream Makefile, stages the binary into `deb/amd64/kloak`, assembles the `deb/package/` filesystem, and runs `dpkg-deb --build`. Output: `./kloak_<version>_amd64.deb`.

## Install

```
sudo apt install ./kloak_0.7.5_amd64.deb
sudo systemctl enable --now kloak
```

Requires a **Wayland session**. Won't do anything useful under X11.

## arm64

Not yet wired up. To add: install a cross-toolchain (or build on an aarch64 host), compile, and add an `arm64` path through `build-deb.sh` that stages into `deb/arm64/` and flips `Architecture:` to `arm64` in a second control file.

## Tracking upstream

```
git remote add upstream https://github.com/Whonix/kloak
git fetch upstream
git merge upstream/master
```

Bump `Version:` in `deb/package/DEBIAN/control` after merging.
