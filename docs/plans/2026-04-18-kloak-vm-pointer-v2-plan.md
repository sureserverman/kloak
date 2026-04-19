# Project Plan: VM-pointer anonymization v2 (split virtual devices)

Date: 2026-04-18

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Restore click-timing anonymization for VM pointer buttons (QEMU USB Tablet, spice vdagent tablet) without cursor chaos, by splitting kloak's single uinput device into a keyboard/relative-mouse sink and a separate absolute-pointer sink.

**Architecture:** Kloak opens two uinput devices — `kloak-kbd` (EV_KEY + EV_REL + EV_MSC, no EV_ABS) and `kloak-pointer` (EV_KEY for mouse buttons + EV_ABS ABS_X/Y + `INPUT_PROP_POINTER`, no EV_REL). Events sourced from `DeviceClass::KeyOrRel` route to `kloak-kbd`; events from `DeviceClass::VmTablet` route to `kloak-pointer`. Libinput's classifier sees two clean devices (one relative, one absolute) instead of one ambiguous REL+ABS hybrid, so ABS events reach the compositor as real pointer motion.

**Tech Stack:** Rust, raw uinput/evdev ioctls, pure-libc FFI. No new runtime deps.

---

## Research Summary

### Why the previous single-device approach failed

- Commit `1ed918a` had kloak advertise EV_REL **and** EV_ABS on one virtual device with `INPUT_PROP_POINTER`. Smoke test with `virsh qemu-agent-command` inside the ubuntu libvirt guest showed the cursor "jumping far away and immediately jumping back" under any ABS motion.
- Root cause: `libinput_device_dispatch` treats a device with both `EV_REL` and `EV_ABS` set as `LIBINPUT_DEVICE_CAPABILITY_POINTER` (relative). ABS events arriving on a relatively-classified device are fed into the pointer-acceleration pipeline inconsistently — motion is sporadically interpreted as delta vs. as warp, producing the jumps we saw.
- Commit `652315b` (skip `BTN_TOOL_*` 0x140..=0x14f) confirmed libinput tablet-classification is separately problematic — we had to hide stylus buttons to avoid "missing tablet capabilities: resolution". That fix is correct and stays.
- Commit `fb561ef` reverted classification to `Skip` for every EV_ABS device. Cursor works because the compositor reads the QEMU USB Tablet directly, but click timing is unanonymized.

### Why two devices fix it

- Libinput classifies each `/dev/input/eventN` independently. `kloak-kbd` advertises no `EV_ABS` → classified as keyboard + relative pointer. `kloak-pointer` advertises no `EV_REL` and has `INPUT_PROP_POINTER` → classified as absolute pointer (mouse, not tablet). Neither device is ambiguous.
- Kernel `uinput` supports any number of virtual devices per process — each is a separate `open("/dev/uinput")` + `UI_DEV_CREATE`. Confirmed by `drivers/input/misc/uinput.c`: the fd is the per-device handle, the driver has no per-process cap.
- udev rules (`/usr/lib/udev/rules.d/60-evdev.rules`, `60-input-id.rules`) tag devices based on per-device capability bits, so separating caps cleanly separates tags. `kloak-pointer` will get `ID_INPUT_MOUSE` (the goal); `kloak-kbd` will get `ID_INPUT_KEYBOARD` and, because of EV_REL, also `ID_INPUT_MOUSE`.

### Project context

- `rust/src/uinput.rs` currently models one `UInput` struct with a single fd. Need to either parameterize it per-sink or wrap two instances.
- `rust/src/queue.rs` (`Scheduler`) emits `ScheduledPacket { sched_time, packet }`. Route info must be carried per packet or resolved at emit time — adding a `sink` tag to the packet is the smallest change.
- `rust/src/evdev.rs` `DeviceClass` enum already has `KeyOrRel` vs. `VmTablet`. Classification is the natural routing key.
- `rust/src/translate.rs` `handle_raw_event` already takes a `TranslateCtx` — extending it with the sink selector for the source device is a 1-field change.
- Dedup logic (`suppress_vm_tablet` in commit `703e6e2`) is retained: when two VM tablets coexist (QEMU + spice vdagent), the second-attached is grabbed but has `abs_x_max = None`, so translate drops its ABS events but the compositor no longer sees raw events from it either.
- Test framework: `cargo test` in `rust/`. Unit tests colocated in each module's `#[cfg(test)] mod tests`. No integration-test harness on disk — VM smoke test is manual via `virsh qemu-agent-command --domain ubuntu`.

### Vault / local docs

- `docs/plans/2026-04-18-kloak-vm-pointer-design.md` — the v1 design. Status: "Implemented" but with caveat that click timing is currently *not* anonymized for VM pointers; this plan supersedes it.
- `docs/plans/` contains six prior plans from the libinput→evdev migration — the staging/rollback pattern there is the house style and this plan mirrors it.

---

## Preflight

- [ ] **Rust toolchain** — `cd rust && cargo --version` runs; `cargo build` on master passes
- [ ] **Baseline tests green** — `cd rust && cargo test` passes on the current tree before any edits
- [ ] **VM harness reachable** — `virsh --connect qemu:///system list --all` shows the `ubuntu` guest; `virsh qemu-agent-command --domain ubuntu '{"execute":"guest-ping"}'` returns without error
- [ ] **Guest build tooling** — inside the guest, `dpkg --version` and `apt-get --version` respond (for installing the rebuilt `.deb`)
- [ ] **Guest has `kloak.service`** — previous build installed; `systemctl status kloak` (inside guest) shows the unit even if inactive

If any preflight check fails, stop and report before touching code.

---

## Stage 1: Parameterize UInput for kbd vs. pointer sinks

**Goal:** `UInput::open` becomes `UInput::open_kbd` / `UInput::open_pointer`, each producing a virtual device with exactly the capability set libinput needs to classify it correctly.
**Depends on:** Preflight green
**Blocks:** Stage 2
**Risk:** LOW — no behavior change yet; only splitting one ioctl sequence into two. Unit-testable by inspecting the capability-setup order.
**Rollback:** `git revert` the Stage 1 commit(s); nothing else touches the tree.

### Task 1.1: Introduce a `SinkKind` enum gating capability setup
- **Files:**
  - Modify: `rust/src/uinput.rs`
- **Depends on:** none
- **Blocks:** Task 1.2, Task 1.3
- **Parallel:** YES
- **Step 1:** Add `pub enum SinkKind { Kbd, Pointer }` near the top of `uinput.rs`.
- **Step 2:** Replace `UInput::open()` with `UInput::open_with(kind: SinkKind) -> io::Result<Self>`. Body branches on `kind`:
  - `Kbd`: set `EV_SYN | EV_KEY | EV_REL | EV_MSC`. Advertise all `KEY_MAX` codes **except** `BTN_TOOL_FIRST..=BTN_TOOL_LAST` (as today). Advertise `REL_X/Y/WHEEL/HWHEEL/*_HI_RES`. Advertise `MSC_SCAN`. Do **not** set `EV_ABS` or `INPUT_PROP_POINTER`. `dev_setup` name: `"kloak-kbd"`.
  - `Pointer`: set `EV_SYN | EV_KEY | EV_ABS`. Advertise only pointer-button codes: `BTN_LEFT (0x110)`, `BTN_RIGHT (0x111)`, `BTN_MIDDLE (0x112)`, `BTN_SIDE (0x113)`, `BTN_EXTRA (0x114)`, `BTN_FORWARD (0x115)`, `BTN_BACK (0x116)`, `BTN_TASK (0x117)`. No other KEY_* codes. Set `INPUT_PROP_POINTER`. Advertise `ABS_X`/`ABS_Y` with `UI_ABS_SETUP` at 0..32767. No `EV_REL`. `dev_setup` name: `"kloak-pointer"`.
- **Step 3:** Add thin wrappers: `pub fn open_kbd() -> io::Result<Self> { Self::open_with(SinkKind::Kbd) }` and `open_pointer()`.
- **Test:** Add unit tests in `uinput.rs`:
  - `sink_kind_kbd_advertises_rel_not_abs` — mock the ioctl helpers via a feature-gated shim (or extract `capability_setup` into a pure function taking a `&mut Vec<(u32, c_int)>` recorder). Assert the recorded calls include `UI_SET_RELBIT` but not `UI_SET_ABSBIT`.
  - `sink_kind_pointer_advertises_abs_not_rel` — inverse.
  - `sink_kind_pointer_skips_non_mouse_keys` — recorder contains `UI_SET_KEYBIT(BTN_LEFT)` and does NOT contain `UI_SET_KEYBIT(KEY_A)` (0x1e).
- **Test command:** `cd rust && cargo test --lib uinput::tests`
- **Expected:** All new tests pass; existing `ioctl_numbers_match_kernel_abi` etc. still pass.
- **Red-Green max cycles:** 3

### Task 1.2: Surface both open entry points to main
- **Files:**
  - Modify: `rust/src/main.rs`
  - Modify: `rust/src/lib.rs` (re-export `SinkKind` if needed)
- **Depends on:** Task 1.1
- **Blocks:** Task 1.3
- **Parallel:** NO (needs 1.1's API)
- **Step 1:** In `main.rs`, change the single `UInput::open()` call site to open **two** handles: `let uinput_kbd = UInput::open_kbd()?;` and `let uinput_pointer = UInput::open_pointer()?;`. Both wrapped with the existing `unwrap_or_else` that prints the uinput-module hint.
- **Step 2:** Keep the rest of the loop unchanged for this stage — route **all** existing packets to `uinput_kbd`. `uinput_pointer` is opened but unused yet.
- **Test:** `cd rust && cargo build` compiles. `cd rust && cargo test` stays green (no behavior change for keyboards/rel mice).
- **Expected:** Build succeeds; tests pass.
- **Red-Green max cycles:** 2

### Task 1.3: Verify both virtual devices appear at runtime
- **Files:** none changed; this is an observation task
- **Depends on:** Task 1.2
- **Blocks:** Stage 2 gate
- **Parallel:** NO
- **Step 1:** Build locally: `cd rust && cargo build --release`.
- **Step 2:** Ship to the libvirt `ubuntu` guest via the existing packaging pipeline (`packaging/ubuntu/build-deb.sh` — or whichever entry point the repo uses; inspect `packaging/` to confirm). Install the `.deb`. Restart `kloak.service`.
- **Step 3:** Inside the guest, run `ls -la /sys/class/input/ | grep kloak` — expect **two** entries whose `device/name` files read `kloak-kbd` and `kloak-pointer`. Run `udevadm info /dev/input/eventN` on each, record the `ID_INPUT_*` tags.
- **Test command:** `virsh qemu-agent-command --domain ubuntu '{"execute":"guest-exec","arguments":{"path":"/bin/sh","arg":["-c","for d in /sys/class/input/event*/device/name; do echo \"$d: $(cat $d)\"; done | grep kloak"],"capture-output":true}}'` then fetch output via `guest-exec-status`.
- **Expected:** Output names both `kloak-kbd` and `kloak-pointer`. `udevadm info` shows `ID_INPUT_MOUSE=1` on `kloak-pointer`, NOT `ID_INPUT_TABLET`.
- **Red-Green max cycles:** 3

### Stage 1 Gate
- [ ] `cargo test` green on host
- [ ] Guest shows both virtual devices under `/sys/class/input/`
- [ ] `udevadm info` on `kloak-pointer` shows `ID_INPUT_MOUSE=1` and *no* `ID_INPUT_TABLET`
- [ ] Keyboard input in the guest still works (regression check — only `kloak-kbd` routes today)
- [ ] Escape combo still exits kloak (sanity check that Stage 1 didn't reorder the key path)

---

## Stage 2: Re-enable VmTablet classification + route per-sink

**Goal:** `DeviceClass::VmTablet` is returned again for QEMU/virtio/spice tablets, and packets originating from those devices are emitted on `kloak-pointer` instead of `kloak-kbd`.
**Depends on:** Stage 1 gate
**Blocks:** Stage 3
**Risk:** MEDIUM — this is where cursor chaos could recur if routing is wrong. Rollback via `git revert` drops us back to the Stage 1 state (VM tablets skipped, keyboard/rel anonymization intact).
**Rollback:** `git revert` the Stage 2 commit(s). Also revert `fb561ef` takeover if merged together. `kloak-pointer` stays opened but unused — no runtime change.

### Task 2.1: Add `Sink` tag to `ScheduledPacket`
- **Files:**
  - Modify: `rust/src/event.rs` (or wherever `InputPacket` lives — inspect `rust/src/` to confirm; likely `event.rs`)
  - Modify: `rust/src/queue.rs`
- **Depends on:** Stage 1 gate
- **Blocks:** Task 2.2, Task 2.3, Task 2.4
- **Parallel:** YES (once Stage 1 is green)
- **Step 1:** Add `#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum Sink { Kbd, Pointer }` in `event.rs`.
- **Step 2:** Extend `ScheduledPacket` with `pub sink: Sink`.
- **Step 3:** Extend every `Scheduler::enqueue_*` method with a `sink: Sink` parameter. Store it in the emitted `ScheduledPacket`. Default is preserved by call sites: most go `Sink::Kbd`; only `enqueue_abs_pos` and `enqueue_button` may take `Sink::Pointer` depending on origin.
- **Step 4:** Update existing `queue.rs` unit tests that construct `ScheduledPacket` inline to pass `sink: Sink::Kbd`.
- **Test command:** `cd rust && cargo test --lib queue::tests event::tests`
- **Expected:** All queue/event unit tests pass.
- **Red-Green max cycles:** 3

### Task 2.2: Translate plumbs a per-device sink
- **Files:**
  - Modify: `rust/src/translate.rs`
  - Modify: `rust/src/evdev.rs` (FrameAccum carries the sink it produces)
- **Depends on:** Task 2.1
- **Blocks:** Task 2.4
- **Parallel:** NO
- **Step 1:** Add `pub sink: Sink` to `FrameAccum`, default `Sink::Kbd`.
- **Step 2:** In `EvdevDevice::open`, when `classify` returns `VmTablet` and the device is NOT suppressed, set `frame.sink = Sink::Pointer`. `KeyOrRel` → `Sink::Kbd` (default).
- **Step 3:** In `handle_raw_event`, pass `accum.sink` into every `ctx.scheduler.enqueue_*(...)` call.
- **Step 4:** Update translate.rs tests to set `h.accum.sink = Sink::Pointer` where they check AbsPos emission, and assert the scheduled packet's sink matches.
- **Test command:** `cd rust && cargo test --lib translate::tests`
- **Expected:** All translate unit tests pass. New assertions confirm VM-tablet-sourced packets carry `Sink::Pointer`.
- **Red-Green max cycles:** 3

### Task 2.3: Re-enable VmTablet classification
- **Files:**
  - Modify: `rust/src/evdev.rs` (revert the Stage-6 `classify` change)
- **Depends on:** Task 2.1 (but not 2.2 — classification is independent)
- **Blocks:** Task 2.4
- **Parallel:** YES (with 2.2, no file overlap)
- **Step 1:** In `classify`, restore the `VmTablet` branch: if `has_key && has_abs && !has_bit(abs_bits, ABS_MT_SLOT)` → `DeviceClass::VmTablet`; `has_key && has_abs && has_bit(abs_bits, ABS_MT_SLOT)` → `Skip`.
- **Step 2:** Update `classify_vm_tablet_is_skipped` test → rename to `classify_vm_tablet_is_vm_tablet` and assert `DeviceClass::VmTablet`.
- **Step 3:** `classify_real_touchpad_has_mt_slot` stays asserting `Skip`.
- **Test command:** `cd rust && cargo test --lib evdev::tests`
- **Expected:** All evdev unit tests pass with the restored VmTablet classification.
- **Red-Green max cycles:** 2

### Task 2.4: Main loop dispatches packets to the correct uinput handle
- **Files:**
  - Modify: `rust/src/main.rs`
- **Depends on:** Task 2.2, Task 2.3
- **Blocks:** Stage 2 gate
- **Parallel:** NO
- **Step 1:** In the emit loop (currently `for sp in scheduler.pop_due(now) { uinput.emit_packet(sp.packet)?; }`), branch on `sp.sink`: `Sink::Kbd` → `uinput_kbd.emit_packet(...)`; `Sink::Pointer` → `uinput_pointer.emit_packet(...)`.
- **Step 2:** Confirm no call site still refers to the old single `uinput` binding; delete it.
- **Test:** `cd rust && cargo build && cargo test` passes. Hand off to Stage 2 gate for runtime verification.
- **Red-Green max cycles:** 2

### Stage 2 Gate
- [ ] `cargo test` green across the whole crate
- [ ] In the guest: rebuild `.deb`, reinstall, restart service. Move the host cursor across the guest window — cursor in the guest follows smoothly (no jumps, no chaos). This is the critical regression check.
- [ ] Click in the guest with the host mouse — click registers; inspect `wev` or equivalent to confirm BTN_LEFT arrives on the `kloak-pointer` device and with visible random delay (compare timestamps of host click vs. guest receipt, using `virsh qemu-agent-command` and host `xdotool click` timing if available).
- [ ] Keyboard input and escape combo still work (regression check).

---

## Stage 3: Harden dedup for multi-tablet VMs

**Goal:** When a guest presents multiple tablet devices (QEMU USB Tablet on event2 AND spice vdagent tablet on event4), kloak grabs both but only forwards one. No duplicate/interleaved AbsPos packets reach `kloak-pointer`.
**Depends on:** Stage 2 gate
**Blocks:** Stage 4
**Risk:** LOW — the logic already exists (`suppress_vm_tablet` / `has_vm_tablet`), this stage just re-enables and tests it.
**Rollback:** Remove the call-sites; defaults to "grab first tablet only," which is already safe.

### Task 3.1: Verify dedup logic still triggers with restored VmTablet
- **Files:**
  - Modify (tests only): `rust/src/evdev.rs`
- **Depends on:** Stage 2 gate
- **Blocks:** Stage 3 gate
- **Parallel:** YES
- **Step 1:** The `suppress_vm_tablet` path was retained when we reverted classification to Skip. Confirm it still does the right thing: when the first tablet is attached and a second call to `attach` arrives for another tablet, `has_vm_tablet()` must return true and the second tablet opens with `abs_x_max = None`.
- **Step 2:** Add a unit test `second_vm_tablet_is_grabbed_but_muted`: construct an `EvdevCtx` with a mock/fake attach path (may require extracting the classification-and-suppress decision into a pure function callable without opening `/dev/input/*`). Feed in two synthetic capability bitmaps, assert the second device's `frame.abs_x_max` is `None`.
- **Test command:** `cd rust && cargo test --lib evdev::tests::second_vm_tablet_is_grabbed_but_muted`
- **Expected:** Test passes.
- **Red-Green max cycles:** 3

### Task 3.2: VM test: two tablets coexist without jumps
- **Files:** none
- **Depends on:** Task 3.1
- **Blocks:** Stage 3 gate
- **Parallel:** NO (requires guest)
- **Step 1:** In the guest, confirm via `ls /sys/class/input/` and `cat /sys/class/input/event*/device/name` that the guest actually has two tablet-like devices (QEMU USB Tablet + spice vdagent or similar). If the `ubuntu` guest only ever has one, document the limitation and skip — the unit test in 3.1 is the load-bearing coverage.
- **Step 2:** If two exist: move the host cursor rapidly across the guest window. Cursor must not jump, oscillate, or lag. Monitor `journalctl -u kloak -f` for any warnings.
- **Test command:** `virsh qemu-agent-command --domain ubuntu '{"execute":"guest-exec","arguments":{"path":"/bin/sh","arg":["-c","ls /sys/class/input/event*/device/name | xargs -I{} sh -c \"echo {}: $(cat {})\""],"capture-output":true}}'`
- **Expected:** Cursor smooth in guest; journal clean.
- **Red-Green max cycles:** 2

### Stage 3 Gate
- [ ] Dedup unit test green
- [ ] In guest with two tablets: no cursor jumps during rapid motion
- [ ] `journalctl -u kloak` shows no warnings about duplicate tablets or failed grabs

---

## Stage 4: VM smoke test — end-to-end click-timing anonymization

**Goal:** Final validation that the stated goal is met: VM pointer button events are scheduled through kloak's random-delay scheduler, cursor motion is smooth, keyboard anonymization is not regressed, and escape combo still works.
**Depends on:** Stage 3 gate
**Blocks:** none (terminal)
**Risk:** LOW — diagnostic only. Failures here feed fixes back into Stage 2 or Stage 3.
**Rollback:** N/A — if this stage reveals a fundamental problem, `git revert` through Stages 2–3; Stage 1 (two uinput devices with only `kloak-kbd` used) is the safe fallback state.

### Task 4.1: Observe button-event delay distribution
- **Files:** none
- **Depends on:** Stage 3 gate
- **Blocks:** Task 4.2
- **Parallel:** YES
- **Step 1:** In the guest, run a small script that logs `evtest /dev/input/by-id/…kloak-pointer…` for 30 seconds. From the host, fire 20 clicks at ~1 Hz via `xdotool click 1` on the guest window.
- **Step 2:** Compare host click timestamps (from `xdotool --version` script log) vs. guest-received timestamps. Compute delays; expect distribution with mean near `max_delay_ms / 2` (default 100 → ~50 ms mean), spread roughly uniform across `[0, max_delay_ms]`.
- **Test command:** `virsh qemu-agent-command --domain ubuntu '{"execute":"guest-exec","arguments":{"path":"/usr/bin/evtest","arg":["/dev/input/by-id/kloak-pointer-event-mouse"],"capture-output":true}}'` — record, then post-process.
- **Expected:** Observed delays match expected distribution. No delay is zero (that would mean no anonymization); no delay is > `max_delay_ms`.
- **Red-Green max cycles:** 2

### Task 4.2: Keyboard regression check
- **Files:** none
- **Depends on:** Task 4.1
- **Blocks:** Task 4.3
- **Parallel:** NO
- **Step 1:** Same methodology as 4.1 but for keyboard keys on `kloak-kbd`. Use `xdotool key a` fired 20 times from host; log on guest.
- **Step 2:** Confirm delays still random in `[0, max_delay_ms]`. This protects against accidental regression of the keyboard path when splitting sinks.
- **Test:** Delay distribution as above.
- **Expected:** Keyboard anonymization unchanged from pre-Stage-1 baseline.
- **Red-Green max cycles:** 2

### Task 4.3: Escape combo still exits kloak
- **Files:** none
- **Depends on:** Task 4.2
- **Blocks:** Stage 4 gate
- **Parallel:** NO
- **Step 1:** From the guest console, press the configured combo (default `KEY_RIGHTSHIFT + KEY_ESC`).
- **Step 2:** Verify `kloak.service` exits cleanly (`systemctl status kloak` shows `inactive (dead)` with exit code 0).
- **Test:** `virsh qemu-agent-command --domain ubuntu '{"execute":"guest-exec","arguments":{"path":"/bin/systemctl","arg":["is-active","kloak"],"capture-output":true}}'` returns "inactive".
- **Expected:** Service exited on the combo.
- **Red-Green max cycles:** 2

### Stage 4 Gate
- [ ] Click delays demonstrate randomization in the expected range
- [ ] Keyboard delays unchanged from baseline (regression-free)
- [ ] Escape combo exits cleanly
- [ ] No warnings in `journalctl -u kloak`
- [ ] Design doc `2026-04-18-kloak-vm-pointer-design.md` updated with a "Superseded by 2026-04-18-kloak-vm-pointer-v2" note, or its status flipped to "Superseded"

---

## Common pitfalls (this plan)

| Pitfall | Guard |
|---|---|
| Advertising `BTN_TOOL_*` on `kloak-pointer` → libinput rejects as incomplete tablet | Task 1.1 explicitly lists the 8 pointer buttons; no loop over `KEY_MAX` on the pointer sink |
| Opening `kloak-pointer` before permissions / uinput module load | Same `unwrap_or_else` as `kloak-kbd` with the clear error message |
| Forgetting to propagate `sink` through scheduler → packets lost or misrouted | Task 2.1 changes the enum exhaustively; compiler flags every call-site |
| Dedup regressing when classification is re-enabled | Task 3.1 adds a unit test specifically for the two-tablet case |
| VM harness unavailable | Preflight catches this before Stage 1 starts; each runtime check has a clear guest-side command |
