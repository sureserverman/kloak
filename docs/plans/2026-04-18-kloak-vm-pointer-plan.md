# VM-pointer ABS passthrough Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make kloak grab QEMU USB Tablet / virtio-tablet devices inside VMM/UTM guests and anonymize their absolute cursor motion, without breaking real-hardware laptop touchpads (which stay untouched).

**Architecture:** At attach time, classify each `/dev/input/eventN` by its capability bitmap. Devices advertising `ABS_MT_SLOT` (real touchpads, Protocol B) are skipped — the compositor handles them directly. Devices advertising `EV_ABS` without `ABS_MT_SLOT` (VM tablets) are grabbed and get their axis limits queried via `EVIOCGABS`. Per-SYN-frame, the latest `ABS_X`/`ABS_Y` values are captured in `FrameAccum`, normalized to 0..32767, and enqueued as a new `InputPacket::AbsPos`. The uinput sink adds EV_ABS capability with `INPUT_PROP_POINTER` to prevent GNOME from tagging it as a drawing tablet.

**Tech Stack:** Rust, libc, nix. Tests with `cargo test`.

**Design doc:** `docs/plans/2026-04-18-kloak-vm-pointer-design.md` (commit `26ed243`).

---

## Task 1: Add `ABS_MT_SLOT` constant and classification helper

**Files:**
- Modify: `rust/src/evdev.rs` (add const; add `classify_device` helper; keep `open()` untouched for now).
- Test: `rust/src/evdev.rs` (existing `#[cfg(test)] mod tests`).

**Step 1: Write the failing test.**

Append to `rust/src/evdev.rs` tests module:

```rust
#[test]
fn classify_vm_tablet_has_abs_no_mt() {
    // QEMU USB Tablet: EV_KEY+EV_ABS, no ABS_MT_SLOT.
    let ev_bits = make_ev_bits(&[EV_KEY, EV_ABS]);
    let abs_bits = make_abs_bits(&[ABS_X, ABS_Y]);
    assert_eq!(classify(&ev_bits, &abs_bits), DeviceClass::VmTablet);
}

#[test]
fn classify_real_touchpad_has_mt_slot() {
    // Laptop touchpad: EV_KEY+EV_ABS and ABS_MT_SLOT set.
    let ev_bits = make_ev_bits(&[EV_KEY, EV_ABS]);
    let abs_bits = make_abs_bits(&[ABS_X, ABS_Y, ABS_MT_SLOT]);
    assert_eq!(classify(&ev_bits, &abs_bits), DeviceClass::Skip);
}

#[test]
fn classify_keyboard_no_abs() {
    let ev_bits = make_ev_bits(&[EV_KEY]);
    let abs_bits = [0u8; 8];
    assert_eq!(classify(&ev_bits, &abs_bits), DeviceClass::KeyOrRel);
}

#[test]
fn classify_rel_mouse() {
    let ev_bits = make_ev_bits(&[EV_KEY, EV_REL]);
    let abs_bits = [0u8; 8];
    assert_eq!(classify(&ev_bits, &abs_bits), DeviceClass::KeyOrRel);
}

#[test]
fn classify_no_ev_key_is_skipped() {
    // Joystick-without-buttons / pure-ABS device.
    let ev_bits = make_ev_bits(&[EV_ABS]);
    let abs_bits = make_abs_bits(&[ABS_X, ABS_Y]);
    assert_eq!(classify(&ev_bits, &abs_bits), DeviceClass::Skip);
}

fn make_ev_bits(types: &[u16]) -> [u8; 4] {
    let mut b = [0u8; 4];
    for &t in types {
        b[(t / 8) as usize] |= 1 << (t % 8);
    }
    b
}

fn make_abs_bits(codes: &[u16]) -> [u8; 8] {
    let mut b = [0u8; 8];
    for &c in codes {
        b[(c / 8) as usize] |= 1 << (c % 8);
    }
    b
}
```

**Step 2: Run the test to verify it fails.**

```bash
cd /home/user/dev/kloak-ubuntu/rust && cargo test classify 2>&1 | tail -20
```
Expected: compile error — `DeviceClass`, `classify`, `ABS_X`, `ABS_Y`, `ABS_MT_SLOT` don't exist yet.

**Step 3: Implement.**

Add near the existing `EV_SYN/KEY/REL/ABS` constants in `rust/src/evdev.rs`:

```rust
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_MT_SLOT: u16 = 0x2f;

/// Coarse classification: after reading the EV_* capability bitmap and (for
/// EV_ABS devices) the ABS code bitmap, what should we do with this device?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceClass {
    /// Keyboard or relative-motion mouse. Grab and translate EV_KEY/EV_REL.
    KeyOrRel,
    /// VM absolute tablet (QEMU USB Tablet / virtio-tablet). Grab and
    /// translate EV_KEY + ABS_X/ABS_Y.
    VmTablet,
    /// Real touchpad, pure-ABS device, or anything else we can't faithfully
    /// mirror. Leave alone so the compositor can talk to it directly.
    Skip,
}

fn classify(ev_bits: &[u8], abs_bits: &[u8]) -> DeviceClass {
    let has_key = has_bit(ev_bits, EV_KEY as usize);
    let has_abs = has_bit(ev_bits, EV_ABS as usize);
    if !has_key {
        return DeviceClass::Skip;
    }
    if !has_abs {
        return DeviceClass::KeyOrRel;
    }
    if has_bit(abs_bits, ABS_MT_SLOT as usize) {
        return DeviceClass::Skip;
    }
    DeviceClass::VmTablet
}
```

**Step 4: Run tests.**

```bash
cargo test classify 2>&1 | tail -20
```
Expected: 5 tests pass.

**Step 5: Commit.**

```bash
cd /home/user/dev/kloak-ubuntu && git add rust/src/evdev.rs && \
  git commit -m "$(cat <<'EOF'
rust/evdev: add DeviceClass classifier

Introduce DeviceClass {KeyOrRel, VmTablet, Skip} and a pure-function
classify() helper keyed on EV_KEY / EV_ABS / ABS_MT_SLOT capability bits.
Not wired into open() yet — that swap happens in a follow-up commit so
classification can be unit-tested on synthetic bitmaps first.
EOF
)"
```

---

## Task 2: Add `EVIOCGABS` ioctl and axis-max query

**Files:**
- Modify: `rust/src/evdev.rs`.

**Step 1: Write the failing test.**

Append to evdev.rs tests:

```rust
#[test]
fn eviocgabs_number_matches_kernel_abi() {
    // EVIOCGABS(ABS_X) = _IOR('E', 0x40 + ABS_X, struct input_absinfo).
    // struct input_absinfo is 6 × 4-byte s32 = 24 bytes on amd64/aarch64.
    assert_eq!(eviocgabs(ABS_X as u8), 0x8018_4540);
    assert_eq!(eviocgabs(ABS_Y as u8), 0x8018_4541);
}
```

**Step 2: Run.**

```bash
cargo test eviocgabs_number 2>&1 | tail -10
```
Expected: compile error, `eviocgabs` undefined.

**Step 3: Implement.**

Add in `rust/src/evdev.rs` near `eviocgbit`:

```rust
/// `struct input_absinfo` exactly as the kernel writes it. All fields
/// are `__s32`, total 24 bytes on every arch kloak targets.
#[repr(C)]
#[derive(Copy, Clone, Default, Debug)]
pub struct InputAbsinfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

/// `EVIOCGABS(abs) = _IOR('E', 0x40 + abs, struct input_absinfo)`.
const fn eviocgabs(abs: u8) -> libc::c_ulong {
    ioc(IOC_READ, b'E', 0x40u16 + abs as u16, size_of::<InputAbsinfo>() as u32) as libc::c_ulong
}

fn query_absinfo(fd: RawFd, abs: u8) -> io::Result<InputAbsinfo> {
    let mut info = InputAbsinfo::default();
    // SAFETY: EVIOCGABS writes exactly size_of::<InputAbsinfo>() bytes into
    // the pointer. `fd` is a live evdev fd.
    let rc = unsafe { libc::ioctl(fd, eviocgabs(abs), &mut info as *mut InputAbsinfo) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(info)
    }
}
```

**Step 4: Run.**

```bash
cargo test eviocgabs_number 2>&1 | tail -10
cargo test --lib 2>&1 | tail -20
```
Expected: ioctl-number test passes; all other tests still green.

**Step 5: Commit.**

```bash
git add rust/src/evdev.rs && git commit -m "$(cat <<'EOF'
rust/evdev: add EVIOCGABS ioctl and InputAbsinfo struct

Needed so the upcoming VM-tablet attach path can read ABS_X/ABS_Y range
limits to normalize raw values into the 0..32767 output space.
EOF
)"
```

---

## Task 3: Wire classifier + ABS-range query into `EvdevDevice::open`

**Files:**
- Modify: `rust/src/evdev.rs` — `EvdevDevice` struct, `open()`, `FrameAccum` interaction.
- Modify: `rust/src/translate.rs` — add `pending_abs_x`, `pending_abs_y` to `FrameAccum`.

**Step 1: Update `FrameAccum` in translate.rs.**

Add to the struct:

```rust
    /// Latest raw ABS_X / ABS_Y seen in the current SYN frame; flushed on
    /// SYN_REPORT. `None` outside of VM-tablet devices.
    pub pending_abs_x: Option<i32>,
    pub pending_abs_y: Option<i32>,
    /// `Some(max)` when the device is a VM tablet; drives normalization.
    /// `None` for keyboards and relative mice.
    pub abs_x_max: Option<i32>,
    pub abs_y_max: Option<i32>,
```

**Step 2: Replace the EV_ABS filter in `EvdevDevice::open`.**

In `rust/src/evdev.rs`, replace:

```rust
if !has_key || has_abs {
    return Ok(None);
}
```

with:

```rust
let abs_bits: [u8; 8] = if has_abs {
    query_bits::<8>(fd, EV_ABS as u8)?
} else {
    [0u8; 8]
};

let class = classify(&ev_bits, &abs_bits);
let (abs_x_max, abs_y_max) = match class {
    DeviceClass::Skip => return Ok(None),
    DeviceClass::KeyOrRel => (None, None),
    DeviceClass::VmTablet => {
        let x_info = query_absinfo(fd, ABS_X as u8)?;
        let y_info = query_absinfo(fd, ABS_Y as u8)?;
        if x_info.maximum <= 0 || y_info.maximum <= 0 {
            // Defensive: a zero/negative range would divide-by-zero later.
            return Ok(None);
        }
        (Some(x_info.maximum), Some(y_info.maximum))
    }
};
```

Then wire `abs_x_max`/`abs_y_max` into the `FrameAccum` construction at the bottom of `open()`:

```rust
let frame = FrameAccum {
    has_hi_res_vwheel,
    has_hi_res_hwheel,
    abs_x_max,
    abs_y_max,
    ..FrameAccum::default()
};
```

**Step 3: Write a test for the VM-tablet attach success path.**

This one needs real ioctl so it goes in an integration test gated on presence of a uinput device — not worth the complexity for this task. Defer to the VM smoke test (Task 8). Unit-test coverage: the classifier tests from Task 1 already cover the decision logic; `open()` is thin glue.

**Step 4: Build and run existing tests.**

```bash
cargo build --lib 2>&1 | tail -15
cargo test --lib 2>&1 | tail -20
```
Expected: compile cleanly; all existing tests still pass.

**Step 5: Commit.**

```bash
git add rust/src/evdev.rs rust/src/translate.rs && git commit -m "$(cat <<'EOF'
rust/evdev: accept VM tablets via classifier + EVIOCGABS

Swap the blanket EV_ABS reject for DeviceClass classification. VM tablets
(EV_KEY + EV_ABS, no ABS_MT_SLOT) get their ABS_X/Y maxima queried and
stashed on FrameAccum for later normalization. Real touchpads remain
filtered so their gestures continue to reach the compositor.
EOF
)"
```

---

## Task 4: Add `InputPacket::AbsPos` variant

**Files:**
- Modify: `rust/src/event.rs`.

**Step 1: Write the failing test.**

Append to `rust/src/event.rs` tests:

```rust
#[test]
fn abs_pos_does_not_coalesce() {
    assert!(!InputPacket::AbsPos { x: 0, y: 0 }.coalesces_with_motion());
}

#[test]
fn abs_pos_display_format() {
    let p = InputPacket::AbsPos { x: 100, y: 200 };
    assert_eq!(format!("{p}"), "AbsPos(x=100, y=200)");
}
```

**Step 2: Run.**

```bash
cargo test abs_pos 2>&1 | tail -10
```
Expected: compile error, `AbsPos` unknown.

**Step 3: Implement.**

In `rust/src/event.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPacket {
    Key { code: u32, pressed: bool },
    Button { code: u32, pressed: bool },
    Motion { dx: i32, dy: i32 },
    Scroll { vert: i32, horiz: i32 },
    /// Absolute pointer position normalized to 0..=32767 on both axes.
    /// Emitted once per SYN frame from VM-tablet devices.
    AbsPos { x: i32, y: i32 },
}
```

Add the matching `Display` arm:

```rust
InputPacket::AbsPos { x, y } => write!(f, "AbsPos(x={x}, y={y})"),
```

`coalesces_with_motion` needs no change — only `Motion` returns `true`.

**Step 4: Run.**

```bash
cargo test --lib 2>&1 | tail -20
```
Expected: abs_pos tests pass; other tests still green. **Other consumers of the enum will warn about missing match arms** — we'll fix them in Tasks 5 and 6.

**Step 5: Commit.**

```bash
git add rust/src/event.rs && git commit -m "$(cat <<'EOF'
rust/event: add InputPacket::AbsPos variant

Absolute pointer position normalized to 0..32767 on both axes. Used by
the VM-tablet passthrough path; other variants unchanged.
EOF
)"
```

---

## Task 5: Add `Scheduler::enqueue_abs_pos`

**Files:**
- Modify: `rust/src/queue.rs`.

**Step 1: Write the failing test.**

Add to `rust/src/queue.rs` tests:

```rust
#[test]
fn enqueue_abs_pos_produces_one_packet() {
    let mut s = Scheduler::new(50);
    let mut rng = FixedRng(10);
    s.enqueue_abs_pos(0, &mut rng, 1000, 2000);
    assert_eq!(s.queue_len(), 1);
    let pkts = s.pop_due(1_000_000);
    assert_eq!(pkts.len(), 1);
    match pkts[0].packet {
        InputPacket::AbsPos { x, y } => {
            assert_eq!(x, 1000);
            assert_eq!(y, 2000);
        }
        _ => panic!("expected AbsPos"),
    }
}
```

(If `FixedRng` doesn't exist in the test module, use whatever pattern the existing queue tests use — read them first.)

**Step 2: Run.**

```bash
cargo test enqueue_abs_pos 2>&1 | tail -10
```
Expected: compile error, `enqueue_abs_pos` doesn't exist.

**Step 3: Implement.**

Add to `Scheduler` impl in `rust/src/queue.rs`, patterned on the existing `enqueue_motion`:

```rust
pub fn enqueue_abs_pos(
    &mut self,
    now: i64,
    rng: &mut dyn RandBetween,
    x: i32,
    y: i32,
) {
    let lb = lower_bound(now, self.prev_release_time, self.max_delay);
    let ub = i64::from(self.max_delay);
    let delay = rng.between(lb, ub);
    let sched_time = now + delay;
    self.prev_release_time = sched_time;
    self.queue.push_back(ScheduledPacket {
        sched_time,
        packet: InputPacket::AbsPos { x, y },
    });
}
```

Note: no coalescing — unlike `Motion`, two queued `AbsPos` are separate cursor samples and must both fire.

**Step 4: Run.**

```bash
cargo test --lib 2>&1 | tail -20
```
Expected: all tests pass.

**Step 5: Commit.**

```bash
git add rust/src/queue.rs && git commit -m "$(cat <<'EOF'
rust/queue: add Scheduler::enqueue_abs_pos

Same delay-computation semantics as enqueue_motion, but no coalescing —
two queued AbsPos samples must both fire so the cursor path through
userspace mirrors the host-side movement.
EOF
)"
```

---

## Task 6: Translate EV_ABS events in `translate.rs`

**Files:**
- Modify: `rust/src/translate.rs`.

**Step 1: Write the failing test.**

Append to translate.rs tests:

```rust
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

#[test]
fn abs_x_y_flush_on_syn_and_normalize() {
    let mut h = Harness::new();
    h.accum.abs_x_max = Some(1000);
    h.accum.abs_y_max = Some(2000);

    h.feed(EV_ABS, ABS_X, 500, false, false); // midpoint of x
    h.feed(EV_ABS, ABS_Y, 1000, false, false); // midpoint of y
    assert_eq!(h.scheduler.queue_len(), 0);
    h.feed(EV_SYN, SYN_REPORT, 0, false, false);

    let pkts = h.scheduler.pop_due(1_000_000);
    assert_eq!(pkts.len(), 1);
    match pkts[0].packet {
        crate::event::InputPacket::AbsPos { x, y } => {
            // 500/1000 * 32767 ≈ 16383; 1000/2000 * 32767 ≈ 16383.
            assert_eq!(x, 16383);
            assert_eq!(y, 16383);
        }
        _ => panic!("expected AbsPos"),
    }
    assert!(h.accum.pending_abs_x.is_none());
    assert!(h.accum.pending_abs_y.is_none());
}

#[test]
fn abs_without_max_is_dropped() {
    // abs_x_max/abs_y_max None → device isn't a VM tablet, any EV_ABS
    // arriving is ignored rather than panicking the normalization.
    let mut h = Harness::new();
    h.feed(EV_ABS, ABS_X, 500, false, false);
    h.feed(EV_SYN, SYN_REPORT, 0, false, false);
    assert_eq!(h.scheduler.queue_len(), 0);
}

#[test]
fn abs_normalization_clamps_above_max() {
    let mut h = Harness::new();
    h.accum.abs_x_max = Some(1000);
    h.accum.abs_y_max = Some(1000);
    h.feed(EV_ABS, ABS_X, 1500, false, false); // above max → clamp to 32767
    h.feed(EV_ABS, ABS_Y, -10, false, false); // below zero → clamp to 0
    h.feed(EV_SYN, SYN_REPORT, 0, false, false);
    let pkts = h.scheduler.pop_due(1_000_000);
    match pkts[0].packet {
        crate::event::InputPacket::AbsPos { x, y } => {
            assert_eq!(x, 32767);
            assert_eq!(y, 0);
        }
        _ => panic!("expected AbsPos"),
    }
}
```

**Step 2: Run.**

```bash
cargo test abs_x_y_flush_on_syn abs_without_max abs_normalization 2>&1 | tail -20
```
Expected: fail (EV_ABS arm is a no-op today).

**Step 3: Implement.**

In `rust/src/translate.rs`, add the constant and arm:

```rust
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
```

In `handle_raw_event`'s match:

```rust
EV_ABS => match code {
    ABS_X => accum.pending_abs_x = Some(value),
    ABS_Y => accum.pending_abs_y = Some(value),
    _ => {} // ABS_MT_*, ABS_PRESSURE, etc. dropped.
},
```

Update `flush_frame`:

```rust
if let (Some(x_max), Some(y_max)) = (accum.abs_x_max, accum.abs_y_max) {
    if accum.pending_abs_x.is_some() || accum.pending_abs_y.is_some() {
        let raw_x = accum.pending_abs_x.unwrap_or(0).clamp(0, x_max);
        let raw_y = accum.pending_abs_y.unwrap_or(0).clamp(0, y_max);
        let x = ((raw_x as i64) * 32767 / x_max as i64) as i32;
        let y = ((raw_y as i64) * 32767 / y_max as i64) as i32;
        ctx.scheduler.enqueue_abs_pos(now, ctx.rng, x, y);
        accum.pending_abs_x = None;
        accum.pending_abs_y = None;
    }
}
```

Also remove the stale `EV_ABS — never reached` module-doc line and replace with the new truth.

**Step 4: Run.**

```bash
cargo test --lib 2>&1 | tail -20
```
Expected: all tests pass, including the three new abs tests.

**Step 5: Commit.**

```bash
git add rust/src/translate.rs && git commit -m "$(cat <<'EOF'
rust/translate: forward EV_ABS ABS_X/Y as AbsPos packets

Accumulate ABS_X/Y into FrameAccum between SYN_REPORTs, normalize against
the per-device max into 0..32767, enqueue as AbsPos. EV_ABS frames on
non-VM-tablet devices (abs_*_max=None) are dropped as a defensive no-op.
EOF
)"
```

---

## Task 7: Extend uinput sink with EV_ABS + INPUT_PROP_POINTER

**Files:**
- Modify: `rust/src/uinput.rs`.

**Step 1: Write the failing test.**

Add to `rust/src/uinput.rs` tests:

```rust
#[test]
fn ui_set_absbit_and_propbit_ioctl_numbers() {
    // UI_SET_ABSBIT = _IOW('U', 103, int) = 0x40045567
    assert_eq!(UI_SET_ABSBIT, 0x4004_5567);
    // UI_SET_PROPBIT = _IOW('U', 110, int) = 0x4004556e
    assert_eq!(UI_SET_PROPBIT, 0x4004_556e);
    // UI_ABS_SETUP = _IOW('U', 4, struct uinput_abs_setup(28 bytes))
    // = 0x401c5504
    assert_eq!(UI_ABS_SETUP, 0x401c_5504);
}

#[test]
fn uinput_abs_setup_layout() {
    // struct uinput_abs_setup: __u16 code + 2 bytes padding + input_absinfo(24) = 28
    assert_eq!(size_of::<UinputAbsSetup>(), 28);
}
```

**Step 2: Run.**

```bash
cargo test ui_set_absbit uinput_abs_setup_layout 2>&1 | tail -15
```
Expected: compile error — the constants and struct don't exist.

**Step 3: Implement.**

Add to `rust/src/uinput.rs`:

```rust
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

/// `INPUT_PROP_POINTER`: tells udev this is a mouse-style device, not a
/// drawing tablet. Without this, advertising EV_ABS + ABS_X/Y causes GNOME
/// Shell to tag the virtual device as ID_INPUT_TABLET (the exact bug the
/// old C daemon hit). With the flag set, udev tags it ID_INPUT_MOUSE and
/// compositors map it to a pointer.
const INPUT_PROP_POINTER: u16 = 0x00;

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct UinputAbsSetup {
    code: u16,
    _pad: u16,
    absinfo: InputAbsinfo,
}

const UI_SET_ABSBIT: u32 = iow(UINPUT_IOCTL_BASE, 103, size_of::<c_int>() as u32);
const UI_SET_PROPBIT: u32 = iow(UINPUT_IOCTL_BASE, 110, size_of::<c_int>() as u32);
const UI_ABS_SETUP: u32 = iow(UINPUT_IOCTL_BASE, 4, size_of::<UinputAbsSetup>() as u32);
```

In `UInput::open`, after `set_evbit(fd, EV_REL)?` add:

```rust
set_evbit(fd, EV_ABS)?;

// Set INPUT_PROP_POINTER so udev tags us as a mouse, not a tablet.
ioctl_int(fd, UI_SET_PROPBIT, c_int::from(INPUT_PROP_POINTER))?;

// Enable ABS_X / ABS_Y and declare their range as 0..32767.
ioctl_int(fd, UI_SET_ABSBIT, c_int::from(ABS_X))?;
ioctl_int(fd, UI_SET_ABSBIT, c_int::from(ABS_Y))?;
abs_setup(fd, ABS_X, 0, 32767)?;
abs_setup(fd, ABS_Y, 0, 32767)?;
```

Add the helper:

```rust
fn abs_setup(fd: RawFd, code: u16, min: i32, max: i32) -> io::Result<()> {
    let setup = UinputAbsSetup {
        code,
        _pad: 0,
        absinfo: InputAbsinfo {
            value: 0,
            minimum: min,
            maximum: max,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        },
    };
    // SAFETY: UI_ABS_SETUP reads exactly size_of::<UinputAbsSetup>() bytes
    // from the pointer; `setup` is fully initialized.
    let rc = unsafe { libc::ioctl(fd, UI_ABS_SETUP as _, &setup as *const UinputAbsSetup) };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
```

Extend `emit_packet`:

```rust
InputPacket::AbsPos { x, y } => {
    self.emit(EV_ABS, ABS_X, x)?;
    self.emit(EV_ABS, ABS_Y, y)?;
}
```

Remove the now-stale comment about EV_ABS being deliberately not advertised and replace with a short note that ABS is enabled with INPUT_PROP_POINTER to dodge the ID_INPUT_TABLET tag.

**Step 4: Run.**

```bash
cargo test --lib 2>&1 | tail -20
cargo clippy --all-targets -- -D warnings 2>&1 | tail -20
cargo fmt --check
```
Expected: all tests pass; clippy clean; fmt clean.

**Step 5: Commit.**

```bash
git add rust/src/uinput.rs && git commit -m "$(cat <<'EOF'
rust/uinput: advertise EV_ABS + ABS_X/Y with INPUT_PROP_POINTER

Enables the kloak virtual device to emit absolute cursor positions for
the VM-tablet passthrough path. INPUT_PROP_POINTER is the key: without
it, advertising EV_ABS/ABS_X/ABS_Y causes udev to tag the device as
ID_INPUT_TABLET and GNOME Shell treats it as a drawing tablet. With
the flag set, udev tags it ID_INPUT_MOUSE and the compositor maps it
to a pointer, which is what QEMU USB Tablet / virtio-tablet need.
EOF
)"
```

---

## Task 8: Full build + VM smoke test

**Files:** none. Validation step only.

**Step 1: Cross-build both arches.**

```bash
cd /home/user/dev/kloak-ubuntu/rust && make x86_64 && ls -l ../deb/amd64/kloak
```
Expected: `install -m 0755 target/x86_64/…/release/kloak …` succeeds; binary exists.

**Step 2: Build the deb.**

```bash
cd /home/user/dev/kloak-ubuntu && dpkg-deb --build deb/package /tmp/kloak_0.7.5_amd64.deb 2>&1 | tail -5
```
Expected: `dpkg-deb: building package 'kloak'` line.

**Step 3: Install in the libvirt VM.**

Follow the pattern used in the last session (Python http.server on 192.168.122.1, then `virsh qemu-agent-command … '{"execute":"guest-exec","arguments":{"path":"/usr/bin/sudo","arg":["dpkg","-i","/tmp/kloak.deb"]}}'`). Verify:

```bash
virsh qemu-agent-command <domain> '{"execute":"guest-exec","arguments":{"path":"/bin/systemctl","arg":["status","kloak"],"capture-output":true}}'
```
Expected: `Active: active (running)`.

**Step 4: Confirm the QEMU tablet was grabbed.**

```bash
virsh qemu-agent-command <domain> \
  '{"execute":"guest-exec","arguments":{"path":"/bin/ls","arg":["-l","/dev/input/by-id"],"capture-output":true}}'
```

Look for `QEMU_USB_Tablet` or `virtio-tablet` symlinks. Then inspect the kloak device's sysfs:

```bash
virsh qemu-agent-command <domain> \
  '{"execute":"guest-exec","arguments":{"path":"/bin/sh","arg":["-c","udevadm info /dev/input/eventN | grep -E ID_INPUT_(TABLET|MOUSE)"],"capture-output":true}}'
```
Expected: kloak's uinput device carries `ID_INPUT_MOUSE=1` and **not** `ID_INPUT_TABLET=1`.

**Step 5: Drive cursor motion.**

Move the host mouse across the VM's display window. The guest cursor should track with the ~25 ms average delay kloak adds. Click BTN_LEFT; guest should see the click.

**Step 6: Regression check — keyboard still works.**

```bash
virsh send-key <domain> KEY_A KEY_B KEY_C
```
Kloak IO counters `wchar` should still tick up (keyboard path unchanged).

**Step 7: Regression check — escape combo still exits.**

```bash
virsh send-key <domain> KEY_RIGHTSHIFT KEY_ESC
```
`systemctl show kloak -p NRestarts` should increment.

**Step 8: Commit a short log of the smoke test (no code changes).**

If all green, just note the result in the commit message of the final code change — no extra doc commit needed.

---

## Task 9: Update design doc header status

**Files:**
- Modify: `docs/plans/2026-04-18-kloak-vm-pointer-design.md` header.

**Step 1:** Change `**Status:** Approved for implementation` to `**Status:** Implemented (commits <first>..<last>)`.

**Step 2: Commit.**

```bash
git commit -am "docs: mark vm-pointer design implemented"
```

---

## Success criteria

- [ ] `cargo test --lib` and `cargo clippy --all-targets -- -D warnings` clean.
- [ ] Both arch builds (`make x86_64`, `make aarch64`) produce binaries.
- [ ] VM smoke test: QEMU tablet is grabbed; guest cursor follows host with kloak delay; keyboard still works; escape combo still exits.
- [ ] `udevadm info` on kloak's uinput device shows `ID_INPUT_MOUSE`, not `ID_INPUT_TABLET`.
- [ ] Real laptop touchpad (if tested on bare metal) is untouched — its `/dev/input/eventN` still speaks directly to the compositor.
