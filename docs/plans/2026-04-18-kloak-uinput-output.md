# kloak uinput output port — implementation plan

**Date:** 2026-04-18
**Goal:** Replace kloak's Wayland-client output path (`zwlr_virtual_pointer_v1` + `zwp_virtual_keyboard_v1`) with a compositor-agnostic uinput output so kloak runs under any Linux graphical stack — GNOME Mutter, KDE KWin, wlroots, Xorg, bare tty.
**Architecture:** Keep the existing libinput input path and jitter buffer verbatim. Rip out the Wayland-client side (protocol bindings, xkbcommon keymap assembly, layer-shell surface, registry plumbing) and replace it with a single `uinput.c` module that opens `/dev/uinput`, declares key/rel/abs event codes, and `write()`s `struct input_event` records. Drop the `find_wl_compositor` Python helper, update systemd unit to be session-independent, update AppArmor profile.
**Tech stack:** C11, libinput, libevdev, Linux uinput (`<linux/uinput.h>`), systemd, AppArmor.

---

## Research summary

### Kernel uinput ABI
- `/dev/uinput` is the userspace entry point. Open it `O_WRONLY | O_NONBLOCK`.
- Per `Documentation/input/uinput.rst`: enable event types via `UI_SET_EVBIT` (e.g. `EV_KEY`, `EV_REL`, `EV_ABS`, `EV_SYN`, `EV_MSC`), then enable each individual code via `UI_SET_KEYBIT`, `UI_SET_RELBIT`, `UI_SET_ABSBIT`. Submit a `struct uinput_setup` via `UI_DEV_SETUP` with `name`, `id.bustype=BUS_VIRTUAL`, `id.vendor/product/version`. Then `UI_DEV_CREATE`. Teardown: `UI_DEV_DESTROY` + `close()`.
- Events are written as `struct input_event { struct timeval time; __u16 type; __u16 code; __s32 value; }`. Always terminate a batch with an `EV_SYN`/`SYN_REPORT`/0.
- Requires **CAP_SYS_ADMIN** on the opening process (already present on kloak's service unit).

### libinput ↔ evdev
- libinput already exposes raw evdev key codes for keyboard events (`libinput_event_keyboard_get_key()` returns `KEY_A` etc.). No translation needed — we emit what we receive, just delayed.
- Pointer: `libinput_event_pointer_get_button()` returns evdev button codes (`BTN_LEFT`…). Pointer motion comes as relative `double` dx/dy which we currently accumulate; uinput takes integer `REL_X`/`REL_Y` so we round/accumulate fractional remainder (same as the existing Wayland path's sub-pixel handling — the relevant accumulator logic stays, only the *sink* changes).
- Touchpad absolute motion maps to `ABS_X`/`ABS_Y` in a normalized range; for uinput we pick `0..65535` and let the kernel's input subsystem scale it.

### Double-input hazard
- kloak grabs input devices exclusively via libinput (`LIBINPUT_GRAB=1` equivalent is implicit when libinput takes evdev exclusive). The grab means the kernel delivers those events only to kloak, not to other clients. Re-emission via uinput goes through a *separate* virtual device, which the kernel then delivers to *all* listeners (including X/Wayland compositors).
- **Failure mode:** if any device fails to grab (e.g. appeared after kloak started, hot-plug), keystrokes will be processed twice. Mitigation: kloak already has a device-add path that grabs new devices on the fly (`libinput_udev_*` backend); this port does not weaken it. We will add a self-test in Stage 5 (type on real keyboard, confirm single character).

### GNOME Mutter compatibility
- Mutter reads `/dev/input/event*` via a uinput-aware layer that treats kloak's virtual device indistinguishably from a hardware keyboard. No protocol work on the GNOME side.
- Systemd service no longer needs `WAYLAND_DISPLAY` or `XDG_RUNTIME_DIR` — kloak becomes a system daemon like `atd`.

### AppArmor
- Current profile permits `/dev/input/event* rw` and `/run/user/*/wayland-* r`. The uinput path replaces wayland rules with a single `/dev/uinput rw` rule. We can drop `/run/user/**` entirely and keep `/proc/**` (libinput still needs it).

---

## Preflight

- [ ] `aarch64-linux-gnu-gcc` is installed (`which aarch64-linux-gnu-gcc`) — for cross-build step in Stage 5
- [ ] `/dev/uinput` is readable by root on the dev host (`ls -l /dev/uinput`) — for Stage 1 smoke test
- [ ] Kernel has `CONFIG_INPUT_UINPUT=y` or module `uinput` loaded (`modprobe uinput; lsmod | grep uinput`)
- [ ] `dpkg-deb --build deb/package deb/amd64/kloak-test.deb` succeeds against current tree — baseline packaging works before we start breaking things
- [ ] Current `make -C c x86_64` succeeds — baseline compile works

---

## Stage 1: uinput output module — standalone

**Goal:** Create a self-contained `uinput.c/h` pair that opens `/dev/uinput`, advertises the event codes kloak needs, and exposes a tiny API. Prove it with a root-run smoke binary that types "hello" into the kernel.
**Depends on:** none
**Blocks:** Stage 2
**Risk:** LOW — well-documented kernel ABI, no kloak integration yet.
**Rollback:** delete the two new files, no other code touched.

### Task 1.1: Write `c/src/uinput.h`
- **Files:** create `c/src/uinput.h`
- **Parallel:** YES
- **Test:** file compiles when `#include`'d from a trivial `.c` that does nothing else (`gcc -c -x c /dev/null -include c/src/uinput.h`).
- **API surface:**
  ```c
  int  uinput_open(void);                                /* returns fd or -1 */
  int  uinput_emit(int fd, uint16_t type, uint16_t code, int32_t value);
  int  uinput_syn(int fd);                               /* SYN_REPORT */
  void uinput_close(int fd);                             /* UI_DEV_DESTROY + close */
  ```
- **Depends on:** none
- **Blocks:** Task 1.2, 1.3

### Task 1.2: Write `c/src/uinput.c`
- **Files:** create `c/src/uinput.c`
- **Parallel:** NO (blocked by 1.1)
- **Test:** compiles against its own header: `gcc -Wall -Wextra -Werror -c c/src/uinput.c -o /tmp/uinput.o`
- **Implementation sketch:** inside `uinput_open`, advertise `EV_KEY` + every code `1..KEY_MAX`, `EV_REL` + `REL_X/Y/WHEEL/HWHEEL`, `EV_ABS` + `ABS_X/Y` with a `uinput_abs_setup` (max 65535), `EV_SYN`, `EV_MSC`+`MSC_SCAN`. Populate `uinput_setup` with name `"kloak"`, `id.bustype=BUS_VIRTUAL`, stable vendor/product. Call `UI_DEV_SETUP` + `UI_DEV_CREATE`.
- **Depends on:** Task 1.1

### Task 1.3: Smoke-test binary
- **Files:** create `c/src/uinput_smoke.c`, add `uinput-smoke` phony target to `c/Makefile`
- **Parallel:** NO (blocked by 1.1)
- **Test:** `sudo ./uinput-smoke` emits the keycodes for "hi\n" and `evtest` (another terminal, pointed at `/dev/input/eventN` where N is the newly-appeared kloak device) shows the corresponding `EV_KEY` events.
- **Depends on:** Task 1.1, Task 1.2
- **Red-Green max cycles:** 3

### Stage 1 Gate
- [ ] `make -C c uinput-smoke` builds cleanly
- [ ] `sudo ./c/uinput-smoke` creates a visible uinput device (`ls /dev/input/by-id/` or `libinput list-devices | grep kloak`)
- [ ] Typed characters from the smoke binary arrive in a focused text field of a GUI app running under GNOME Mutter — end-to-end proof the approach works on the target compositor

---

## Stage 2: Replace Wayland emit calls in kloak.c with uinput calls

**Goal:** Keep kloak.c compiling and running, but swap every `zwlr_virtual_pointer_v1_*` / `zwp_virtual_keyboard_v1_*` call with an equivalent `uinput_emit` call. Wayland client code still present for now; only the sink changes.
**Depends on:** Stage 1 gate
**Blocks:** Stage 3
**Risk:** MEDIUM — ~25 call sites in kloak.c; some bundle multiple Wayland calls (e.g. pointer frame + axis source) that need to be unified into a single uinput SYN.
**Rollback:** `git checkout c/src/kloak.c` — Stage 1's uinput.c/h/smoke stay.

### Task 2.1: Add uinput fd to `state` struct, open on startup, close on teardown
- **Files:** modify `c/src/kloak.h` (add `int uinput_fd`), `c/src/kloak.c` (call `uinput_open` in main before Wayland init; call `uinput_close` in cleanup path)
- **Test:** kloak starts as root, `ls /dev/input/by-id/` shows a `kloak` virtual device alongside the real keyboard.
- **Depends on:** Stage 1 gate

### Task 2.2: Keyboard re-emission
- **Files:** modify `c/src/kloak.c` around line 2004 (`zwp_virtual_keyboard_v1_modifiers` / `_key`)
- **Test:** type in a GNOME Mutter text editor while kloak runs with `--delay 100`. Characters appear with measurable ~100ms lag. No doubled characters.
- **Depends on:** Task 2.1

### Task 2.3: Pointer button re-emission
- **Files:** modify `c/src/kloak.c` around lines 1967, 1971 (`zwlr_virtual_pointer_v1_button`)
- **Test:** left/right click in GNOME works, delayed by the configured amount.
- **Depends on:** Task 2.1

### Task 2.4: Pointer motion (relative + absolute) and axis
- **Files:** modify `c/src/kloak.c` around lines 2052, 2334–2384 (`_motion_absolute`, `_axis`, `_axis_discrete`, `_axis_source`, `_frame`)
- **Test:** mouse cursor moves smoothly under GNOME with the configured delay. Scroll wheel works.
- **Depends on:** Task 2.1

### Stage 2 Gate
- [ ] `make -C c x86_64` builds cleanly
- [ ] Local install (`sudo dpkg-deb --build deb/package && sudo dpkg -i …`) starts the service
- [ ] Keyboard + mouse behave normally under GNOME Mutter with kloak active
- [ ] `sudo systemctl status kloak` shows no errors in the log
- [ ] Existing Wayland-client code still in the binary but no longer load-bearing — kloak works even if we kill the compositor it connected to (it won't — we haven't dropped that code yet, but events continue flowing through uinput)

---

## Stage 3: Strip Wayland client code

**Goal:** Delete every line of Wayland-client code from kloak. Remove protocol XML + generated `.c/.h` + Makefile rules. Drop `xkbcommon`, `wayland-client` from pkg-config. After this stage kloak is a pure libinput → uinput pipeline.
**Depends on:** Stage 2 gate
**Blocks:** Stage 4
**Risk:** MEDIUM — big deletion (~1900 lines). Easy to leave orphan symbols or dead includes that break the build.
**Rollback:** `git checkout c/`. Stage 1+2 are preserved.

### Task 3.1: Delete protocol bindings from kloak.c
- **Files:** modify `c/src/kloak.c` — remove `registry_listener_*`, `bind_*`, `keymap_*`, `layer_surface_configure`, `layer_surface_closed`, output handlers, all `wl_registry_bind` calls (lines ~1120–1225 and their helpers). Remove the main loop's `wl_display_dispatch`.
- **Test:** `make -C c x86_64` compiles with zero warnings about unused functions.
- **Depends on:** Stage 2 gate

### Task 3.2: Delete layer-shell surface and shm buffer code
- **Files:** modify `c/src/kloak.c` — remove `layer->*` references, shm pool creation, surface commit paths.
- **Test:** compiles; `ldd ./c/kloak | grep -E "wayland|xkbcommon"` returns nothing.
- **Depends on:** Task 3.1

### Task 3.3: Delete protocol XMLs, generated code, Makefile rules
- **Files:** delete `c/protocol/` directory, `c/src/xdg-shell-protocol.*`, `c/src/xdg-output-protocol.*`, `c/src/wlr-layer-shell.*`, `c/src/wlr-virtual-pointer.*`, `c/src/virtual-keyboard.*`. Modify `c/Makefile` — remove wayland-scanner rules, remove `wayland-client` and `xkbcommon` from pkg-config invocation, shrink the `kloak:` target's dependency list.
- **Test:** `make -C c clean && make -C c x86_64` builds cleanly. `ldd ./c/kloak` shows only libevdev, libinput, libm, libc.
- **Depends on:** Task 3.2

### Task 3.4: Delete gitignore entries and sync-upstream MAP entries for protocol-generated files
- **Files:** modify `.gitignore` (drop `c/src/xdg-shell-protocol.[ch]`, etc.), `sync-upstream.sh` (drop `[protocol]` from MAP so upstream protocol/ changes no longer land in our tree)
- **Test:** `git status` is clean after a `make -C c clean && make -C c x86_64 && git status`.
- **Depends on:** Task 3.3

### Stage 3 Gate
- [ ] `ldd ./c/kloak` shows no wayland-* or xkbcommon dependency
- [ ] `make -C c aarch64` cross-builds cleanly (the sysroot no longer needs wayland/xkbcommon — but we leave the EXCLUDES list alone; unused deps don't hurt)
- [ ] kloak still obfuscates keystrokes under GNOME Mutter after a full rebuild + reinstall
- [ ] Binary size dropped meaningfully (`ls -l ./c/kloak` before/after: expect 30–40% shrink)

---

## Stage 4: Packaging — systemd unit, AppArmor, control Depends, helper removal

**Goal:** Make the deb package reflect the new runtime model. Service no longer waits on a compositor; AppArmor profile drops Wayland rules; `find_wl_compositor` Python helper is removed; Depends line loses `libwayland-client0` + `libxkbcommon0`.
**Depends on:** Stage 3 gate
**Blocks:** Stage 5
**Risk:** LOW — file edits, no code.
**Rollback:** `git checkout deb/`.

### Task 4.1: Rewrite systemd service unit
- **Files:** modify `deb/package/usr/lib/systemd/system/kloak.service`
- **Changes:** drop `After=graphical.target`, `ExecStartPre=…find_wl_compositor`, `EnvironmentFile=-/run/kloak_wl_compositor_data`. Add `After=systemd-udevd.service`, `Requires=systemd-udevd.service`. Change `WantedBy=graphical.target` → `WantedBy=multi-user.target`. Add `DeviceAllow=/dev/uinput rw` and `DeviceAllow=/dev/input/event* rw`.
- **Test:** `systemd-analyze verify deb/package/usr/lib/systemd/system/kloak.service` returns no errors.
- **Depends on:** Stage 3 gate

### Task 4.2: Rewrite AppArmor profile
- **Files:** modify `deb/package/etc/apparmor.d/usr.bin.kloak`
- **Changes:** drop `/run/user/*/wayland-* r`, `/run/user/*/ r`, `network unix stream`. Add `/dev/uinput rw`.
- **Test:** `apparmor_parser -Q deb/package/etc/apparmor.d/usr.bin.kloak` parses successfully.
- **Depends on:** Stage 3 gate

### Task 4.3: Remove find_wl_compositor helper
- **Files:** delete `deb/package/usr/libexec/kloak/find_wl_compositor` and remove `[usr/libexec/kloak]` from `sync-upstream.sh`'s MAP (no longer our concern upstream).
- **Test:** `ls deb/package/usr/libexec/kloak/` is empty (or the directory is removed entirely).
- **Depends on:** Stage 3 gate

### Task 4.4: Update control Depends
- **Files:** modify `deb/package/DEBIAN/control`
- **Changes:** `Depends: libevdev2, libinput10` (drop `libwayland-client0, libxkbcommon0`; no longer need `kbd` or `python3` since `find_wl_compositor` is gone).
- **Test:** `dpkg-deb --build deb/package /tmp/kloak-test.deb && dpkg-deb -I /tmp/kloak-test.deb | grep Depends` shows the new line.
- **Depends on:** Task 4.3

### Task 4.5: Update README-UBUNTU.md
- **Files:** modify `README-UBUNTU.md`
- **Changes:** remove the "Wayland-only, wlroots-family compositors" caveat. Add a one-liner: "Works on any Linux graphical stack — GNOME, KDE, Xorg, wlroots — because output goes through a kernel uinput device." Update the dependency list and tracking-upstream section (mention that the protocol/ map entry was removed).
- **Test:** a human reads it end-to-end and it makes sense.
- **Depends on:** Task 4.4

### Stage 4 Gate
- [ ] `systemd-analyze verify` clean
- [ ] `apparmor_parser -Q` clean
- [ ] `dpkg-deb --build deb/package` succeeds, new control looks right
- [ ] `sudo dpkg -i kloak_*.deb && sudo systemctl start kloak && sudo systemctl status kloak` on a GNOME Ubuntu VM shows active/running with no Wayland-related log lines
- [ ] Typing in GNOME text editors shows the configured delay

---

## Stage 5: Full-pipeline verification and publish dry-run

**Goal:** Exercise the whole publish pipeline end-to-end. Build amd64 + arm64 via `make -C c x86_64 aarch64`, dry-run the publish script (PATH shim, no rsync to the actual server), and confirm a GNOME Ubuntu VM installs and runs the result.
**Depends on:** Stage 4 gate
**Blocks:** nothing (ship gate)
**Risk:** LOW — the build/publish machinery was validated in the prior project; this stage re-verifies against the reshaped tree.
**Rollback:** N/A — if this fails the fix belongs in Stage 1–4.

### Task 5.1: Clean build both arches
- **Test:** `make -C c clean && make -C c x86_64 && make -C c aarch64` both succeed. `file deb/amd64/kloak` says x86-64, `file deb/arm64/kloak` says aarch64. No leftover `.h` / `.c` in `c/src/` beyond `kloak.[ch]` + `uinput.[ch]` (+ smoke if we kept it).
- **Depends on:** Stage 4 gate

### Task 5.2: Dry-run publish
- **Test:** `PATH=/tmp/publish-shim:$PATH ~/dev/utils/publish kloak-ubuntu` completes with both amd64 and arm64 artifacts built and the rsync step intercepted by the shim (no production changes).
- **Depends on:** Task 5.1

### Task 5.3: Install on GNOME Ubuntu and verify
- **Test:** `sudo dpkg -i kloak_*_amd64.deb && sudo systemctl enable --now kloak`. In a GNOME text editor, typing "hello" with `--delay 100` shows each character appearing ~100ms late. No doubled characters. `journalctl -u kloak` is quiet.
- **Depends on:** Task 5.1

### Stage 5 Gate
- [ ] Publish dry-run green for this package
- [ ] Existing `exit-node-flag` (rust) regression check still green when publish is invoked against it — uinput port didn't break unrelated pipeline bits
- [ ] GNOME Ubuntu hands-on test passes
- [ ] Binary size, dependency list, service unit, apparmor profile all reflect the new simpler design
- [ ] `.upstream-sync` is bumped (or noted as "uinput fork diverges from upstream Wayland path — sync manually as needed")

---

## Notes on upstream relationship

After this plan lands, our fork diverges from upstream Whonix/kloak materially — we no longer track `protocol/` or `usr/libexec/kloak/find_wl_compositor`, and `src/kloak.c` has a different output backend. `sync-upstream.sh` will still pick up upstream fixes in shared territory (libinput input path, jitter logic, the Makefile warnings), but `src/kloak.c` changes will increasingly hit the 3-way merge path. That's acceptable — the whole point of this work is to support a use case upstream declined.
