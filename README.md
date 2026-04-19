# kloak for Ubuntu

Ubuntu / Debian `.deb` build of [Whonix/kloak](https://github.com/Whonix/kloak)
— a keystroke and mouse-movement anonymizer that defends against keystroke
biometric profiling by randomizing input event timing. This fork re-injects
events through the kernel's `/dev/uinput` instead of upstream's wlroots
virtual-pointer/virtual-keyboard Wayland protocols, so it works under GNOME
Mutter, KDE KWin, wlroots compositors, and Xorg alike.

This fork repackages upstream into the multi-project deb pipeline used across
`~/dev/*` and adds a self-contained arm64 cross-build path that requires no
system-wide apt configuration.

---

## Quick start

```bash
# one-time host setup (cross-toolchain, build deps)
make -C c bootstrap

# build + publish both archs to the local reprepro repo + rsync to remote
cd ~/dev/utils && ./publish kloak-ubuntu

# install the resulting .deb on a target machine
sudo apt install ./kloak_0.7.5_amd64.deb
sudo systemctl enable --now kloak
```

That's the entire flow. Everything below is detail on what those commands do
and how the pieces fit together.

---

## Repository layout

```
kloak-ubuntu/
├── c/                              language-scoped source + Makefile
│   ├── src/                        C source (kloak.c + uinput.[ch])
│   ├── man/                        ronn man-page source
│   ├── Makefile                    upstream make rules + publish-toolkit
│   │                               targets (x86_64, aarch64, bootstrap)
│   ├── build-arm64-sysroot.sh      fetches arm64 dev libs into a local
│   │                               sysroot from ports.ubuntu.com
│   └── sysroot-arm64/              (gitignored) arm64 cross-build sysroot
│
├── deb/                            publish-pipeline staging area
│   ├── amd64/                      x86_64 binary staged here by `make x86_64`
│   ├── arm64/                      aarch64 binary staged here by `make aarch64`
│   └── package/                    skeleton .deb tree (committed)
│       ├── DEBIAN/{control,postinst,prerm}
│       ├── etc/apparmor.d/         apparmor profiles
│       └── usr/
│           ├── bin/kloak           (build output, gitignored)
│           ├── lib/systemd/system/kloak.service
│           └── share/man/man8/kloak.8.gz   (build output, gitignored)
│
├── debian/                         debhelper tree inherited from Whonix
│                                   (parallel debuild flow — NOT used by
│                                   `~/dev/utils/publish`; see below)
│
├── build.sh                        CodeQL autobuild entry point
└── README.md                       this file
```

The `c/` + `deb/{amd64,arm64,package}/` layout matches the convention shared
by every other publishable-deb project under `~/dev/`. Rust projects use
`rust/Makefile`; this is the C analog.

---

## Build & publish

The build is driven by `~/dev/utils/publish`:

```bash
cd ~/dev/utils && ./publish kloak-ubuntu
```

What that does, end-to-end:

1. Discovers `c/Makefile` (publish probes for `rust/Makefile` then `c/Makefile`).
2. For each arch in `(amd64, arm64)`:
   - Invokes `make -C c x86_64` or `make -C c aarch64`.
   - That target clean-builds the binary, builds the man page (amd64 only),
     and stages the output at `deb/<arch>/kloak`.
3. Copies `deb/package/` to a temp dir, overlays the staged binary into
   `usr/bin/kloak`, patches `Architecture:` in `DEBIAN/control`, and runs
   `dpkg-deb --build`.
4. For each distro listed in `~/dev/utils/distr.list`, runs
   `reprepro includedeb` against `/var/www/repository/`.
5. `rsync`s the entire repo tree to the remote host.

For an amd64-only build with no cross-toolchain involvement:

```bash
make -C c x86_64           # produces deb/amd64/kloak
```

---

## arm64 cross-compile (self-contained)

The naive Debian/Ubuntu cross-build approach (`dpkg --add-architecture arm64`
+ apt-installing `:arm64` dev packages) requires modifying
`/etc/apt/sources.list.d/ubuntu.sources` to route arm64 fetches to
`ports.ubuntu.com`, which is intrusive system-wide configuration.

This project sidesteps that completely. The flow is:

**One-time** — `make -C c bootstrap` installs only host-side packages:

- `build-essential`, `pkg-config`, `ronn`
- amd64 dev libs for the host build (`libevdev-dev`, `libinput-dev`)
- `crossbuild-essential-arm64` (the cross-toolchain — installed as amd64,
  contains the aarch64 GCC and bundled cross-libc under
  `/usr/aarch64-linux-gnu/`)

No `dpkg --add-architecture`, no `:arm64` apt packages, no `/etc/apt/`
edits, no foreign architecture registration.

**On first `make -C c aarch64`** — `c/build-arm64-sysroot.sh` runs
automatically:

1. Downloads `Packages.gz` indexes from `ports.ubuntu.com` for
   `noble{,-updates,-security}` × `{main,universe}` via plain `curl`.
2. Resolves the dependency closure of `libevdev-dev`, `libinput-dev`.
3. Excludes packages provided by the cross-toolchain (libc, libgcc,
   libstdc++) and large unused transitives (python, glib, openssl).
4. Fetches each `.deb` directly from `ports.ubuntu.com` via `curl`.
5. Extracts everything with `dpkg-deb -x` into `c/sysroot-arm64/root/`.

The Makefile's `aarch64` target then cross-compiles with:

```
CC                     = aarch64-linux-gnu-gcc
PKG_CONFIG_LIBDIR      = c/sysroot-arm64/root/usr/lib/aarch64-linux-gnu/pkgconfig:...
PKG_CONFIG_SYSROOT_DIR = c/sysroot-arm64/root
LDFLAGS               += -L<sysroot>/usr/lib/aarch64-linux-gnu
                        -Wl,-rpath-link,<sysroot>/usr/lib/aarch64-linux-gnu
                        -Wl,--allow-shlib-undefined
```

`--allow-shlib-undefined` lets the link complete despite glib symbols
referenced via transitive `DT_NEEDED` (libwacom → glib, libgudev → glib);
the dynamic loader resolves them at runtime on the real arm64 system.

To rebuild the sysroot from scratch (e.g. after a noble point release):

```bash
rm -rf c/sysroot-arm64
make -C c aarch64        # triggers build-arm64-sysroot.sh
```

---

## Install

```bash
sudo apt install ./kloak_0.7.5_amd64.deb       # or ..._arm64.deb
sudo systemctl enable --now kloak
```

Verify the daemon is running:

```bash
systemctl status kloak
journalctl -u kloak -f
```

kloak grabs `/dev/input/event*` keyboards and mice, runs every event
through a randomized-delay buffer, and re-emits the buffered stream via
`/dev/uinput`. This is compositor-agnostic — it works under GNOME Mutter,
KDE KWin, wlroots compositors, and Xorg. The service requires
`CAP_SYS_ADMIN` (for `EVIOCGRAB`) and access to `/dev/uinput`.

---

## Two parallel packaging systems

This repo contains **both** packaging trees:

|                 | `debian/` (upstream)                                      | `deb/` (this fork)                                    |
|-----------------|-----------------------------------------------------------|-------------------------------------------------------|
| Origin          | Inherited from Whonix                                     | Added by this fork                                    |
| Build tool      | `debuild` / `dpkg-buildpackage` (debhelper)               | `dpkg-deb --build`, driven by `~/dev/utils/publish`   |
| Driven by       | `debian/rules`                                            | `c/Makefile` + `~/dev/utils/publish`                  |
| Output          | source `.dsc` + binary `.deb`s + `.changes`               | one binary `.deb` per arch                            |
| Target          | upload to a Debian/Ubuntu archive                         | local `reprepro` repo, rsync'd to a private mirror    |
| Used by publish | no                                                        | yes                                                   |
| Build deps      | declared in `debian/control`, enforced by debhelper       | managed by `make -C c bootstrap`                      |

The `debian/` tree is left in place so the upstream debuild flow remains
usable for anyone who wants it. The publish pipeline ignores it entirely.

---

## Maintenance reference

| Task                                  | Command                                                      |
|---------------------------------------|--------------------------------------------------------------|
| Build amd64 only                      | `make -C c x86_64`                                           |
| Build arm64 only                      | `make -C c aarch64`                                          |
| Generate man page                     | `make -C c man`                                              |
| Clean build artifacts                 | `make -C c clean`                                            |
| Rebuild arm64 sysroot from scratch    | `rm -rf c/sysroot-arm64 && make -C c aarch64`                |
| Full publish (both archs, all distros)| `cd ~/dev/utils && ./publish kloak-ubuntu`                   |
| Bump version                          | edit `Version:` in `deb/package/DEBIAN/control`              |

---

## Project conventions

This project conforms to the multi-project deb-packaging convention used
across `~/dev/*`. See `~/dev/utils/README.md` for the publish pipeline
documentation. Briefly:

- **Source under `<lang>/`** — rust projects use `rust/`, C projects use
  `c/`. The Makefile lives there, not at repo root.
- **`deb/package/` is the committed `.deb` skeleton** — DEBIAN control
  files plus the filesystem mirror (`etc/`, `usr/lib/systemd/`,
  `usr/libexec/`, etc.). Build outputs (`usr/bin/`, `usr/share/man/`)
  are gitignored.
- **`deb/<arch>/` holds per-arch staged binaries** — populated by the
  Makefile's `x86_64` / `aarch64` targets, consumed by `publish` when
  assembling the `.deb`.
- **Every project supports both archs unless infeasible** — see the
  arm64 cross-compile section above for how this project handles it
  without polluting host system configuration.
