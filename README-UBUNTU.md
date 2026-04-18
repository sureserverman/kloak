# kloak for Ubuntu

Ubuntu `.deb` build of [Whonix/kloak](https://github.com/Whonix/kloak) — keystroke and mouse-movement anonymization via timing randomization. Wayland-only.

## Layout

Conforms to the project's standard publishable-deb layout (rust projects use `rust/Makefile`; this is the C analog):

```
kloak-ubuntu/
├── c/
│   ├── src/                         upstream C source
│   ├── protocol/                    upstream Wayland protocol XML
│   ├── man/                         ronn man-page source
│   └── Makefile                     upstream make rules + publish-toolkit
│                                    targets (x86_64, aarch64, bootstrap)
├── deb/
│   ├── amd64/                       compiled binary staged here by `make x86_64`
│   ├── arm64/                       compiled binary staged here by `make aarch64`
│   └── package/
│       ├── DEBIAN/{control,postinst,prerm}
│       ├── etc/apparmor.d/          apparmor profiles
│       └── usr/
│           ├── bin/kloak            (build output, gitignored)
│           ├── libexec/kloak/find_wl_compositor
│           ├── lib/systemd/system/kloak.service
│           └── share/man/man8/kloak.8.gz   (build output, gitignored)
├── debian/                          upstream Whonix debhelper tree (parallel
│                                    debuild flow; not used by publish)
└── README-UBUNTU.md
```

## Build & publish

The build is driven by `~/dev/utils/publish` (see `~/dev/utils/README.md`):

```bash
cd ~/dev/utils && ./publish kloak-ubuntu
```

This compiles for both `amd64` and `arm64`, builds two `.deb`s, includes them in `reprepro` for every distro listed in `distr.list`, and rsyncs the repo to the remote host.

For amd64 only (no cross-toolchain needed), `make -C c x86_64` produces `deb/amd64/kloak`.

## arm64 (cross-compile)

One-time setup:

```bash
make -C c bootstrap
```

This adds `arm64` as a foreign Debian architecture and installs the host build deps plus `crossbuild-essential-arm64` and `:arm64` dev libs for `libevdev`, `libinput`, `libwayland`, `libxkbcommon`. Uses `sudo`.

After bootstrap, `make -C c aarch64` (or the publish run above) cross-compiles for `aarch64-linux-gnu` and stages the binary at `deb/arm64/kloak`.

## Install

```bash
sudo apt install ./kloak_0.7.5_amd64.deb
sudo systemctl enable --now kloak
```

Requires a **Wayland session**. Won't do anything useful under X11.

## Tracking upstream

Use the selective sync script — it manages its own `upstream` remote
(pointing at `https://github.com/Whonix/kloak`, independent of whatever
`origin` points to in your fork) and copies only the upstream paths we
use into our restructured layout (`c/src`, `c/protocol`, `c/man`,
`deb/package/etc`, `deb/package/usr`). It 3-way merges `Makefile` so the
publish-toolkit targets at the bottom survive upstream edits.

```bash
./sync-upstream.sh           # check what's new, no writes
./sync-upstream.sh pull      # apply: copy + merge + bump .upstream-sync
```

The last-applied upstream commit is recorded in `.upstream-sync` (tracked).
After a sync, mirror any `debian/changelog` Version bump into
`deb/package/DEBIAN/control` and rebuild via `cd ~/dev/utils && ./publish kloak-ubuntu`.
