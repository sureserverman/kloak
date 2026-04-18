# Design: VM-pointer ABS passthrough (Approach A1)

**Date:** 2026-04-18
**Status:** Approved for implementation
**Scope:** Make kloak-ubuntu usable inside libvirt/VMM and UTM (macOS QEMU) guests, whose pointer device is an emulated absolute tablet that the current evdev filter rejects.

---

## Problem

The libinput → hand-rolled evdev swap (commit `82fca32`) added a device filter that rejects any `/dev/input/eventN` advertising `EV_ABS`, because our uinput sink speaks only `EV_KEY` + `EV_REL`. That filter drops the QEMU USB Tablet (used by libvirt/virt-manager and UTM) and its virtio-tablet sibling: both expose only `ABS_X` and `ABS_Y` with no multitouch, and both are the *sole* pointer in a typical VM guest. Result: inside a VM, kloak ignores the pointer entirely — cursor motion from the host never reaches the compositor via kloak.

## Goal

Inside VMM/UTM guests, kloak grabs the emulated tablet, anonymizes its motion timing, and emits anonymized motion via a uinput device the guest compositor treats as a pointer.

## Non-goals

- Real-hardware touchpad anonymization (laptop Synaptics/Elan with multitouch Protocol B). Out of scope; real touchpads remain untouched and route to the compositor directly.
- Tablets with pressure/tilt (Wacom), joysticks, touchscreens.
- ABS-MT gesture detection inside kloak.

## Architecture

### Device classification at attach time (new)

`EvdevDevice::open` currently rejects on `has_abs`. Replace with:

| Capability signature | Treatment |
|---|---|
| Has `EV_KEY`, no `EV_ABS` | Grab (keyboard / relative mouse) — unchanged. |
| Has `EV_KEY`, has `EV_ABS`, **no** `ABS_MT_SLOT` | Grab as **VM-tablet** — new. |
| Has `EV_KEY`, has `EV_ABS`, **has** `ABS_MT_SLOT` | Skip (real touchpad) — leave for compositor. |
| No `EV_KEY` | Skip — unchanged. |

`ABS_MT_SLOT` (code `0x2f`) is the definitive Protocol-B marker. QEMU USB Tablet and virtio-tablet do not advertise it; real touchpads always do.

### Per-device metadata

Extend `EvdevDevice` with a `kind` enum and an optional axis-range struct:

```rust
enum DeviceKind { Keyboard, RelMouse, VmTablet { x_max: i32, y_max: i32 } }
```

On attach for a VM tablet, query `EVIOCGABS(ABS_X)` and `EVIOCGABS(ABS_Y)` once, store `maximum` values. Kloak normalizes incoming coordinates against those to emit on a fixed output range (see below).

### Frame accumulator

`FrameAccum` gains two fields:

```rust
pending_abs_x: Option<i32>,
pending_abs_y: Option<i32>,
```

On `EV_ABS` with code `ABS_X` or `ABS_Y`, store into the accumulator. On `SYN_REPORT`, if either is `Some`, flush as an `InputPacket::AbsPos`. Other `EV_ABS` codes (MT, pressure, tilt) are dropped at translate time — they can't reach us anyway because we filter MT devices at attach.

### New InputPacket variant

```rust
InputPacket::AbsPos { x: i32, y: i32 }
```

`x` and `y` are already-normalized into `0..=32767` before enqueueing. Normalization formula per axis: `(raw * 32767) / source_max`. Stored in the packet; scheduler and output are axis-range-agnostic.

### Uinput sink extensions

Advertise `EV_ABS` with `UI_SET_EVBIT`, enable `ABS_X` and `ABS_Y` via `UI_SET_ABSBIT`, and call `UI_ABS_SETUP` for each with `minimum=0, maximum=32767, fuzz=0, flat=0, resolution=0`.

Set `INPUT_PROP_POINTER` via `UI_SET_PROPBIT` (ioctl base `U`, nr `0x86`). This is the critical bit that keeps udev from tagging the device `ID_INPUT_TABLET` — the C daemon's old bug was advertising EV_ABS without this property, which made GNOME Shell treat the virtual device as a drawing tablet.

`emit_packet` gains an arm:

```rust
InputPacket::AbsPos { x, y } => {
    self.emit(EV_ABS, ABS_X, x)?;
    self.emit(EV_ABS, ABS_Y, y)?;
}
```

followed by the usual `SYN_REPORT`.

### Scheduler semantics

AbsPos frames are randomized per-SYN-frame like `Motion` is today: one scheduled packet per SYN_REPORT, random delay `[0, max_delay_ms]`. No per-event reordering within a frame (there is nothing to reorder — x and y emit together).

There is one subtlety: `Motion` accumulates multiple REL deltas and emits a sum, so reordering is harmless. `AbsPos` emits a point, and reordering two scheduled AbsPos packets in time would move the cursor backwards before forwards. The existing scheduler is already a min-heap keyed on scheduled-emit time, so ordering is preserved within one device; this works without change.

## Data flow

```
/dev/input/eventN (VM tablet)
   │  EV_ABS ABS_X, EV_ABS ABS_Y, EV_KEY BTN_LEFT, EV_SYN SYN_REPORT
   ▼
evdev.rs drain_into
   │  (type, code, value) tuples
   ▼
translate.rs handle_raw_event
   │  - EV_ABS ABS_X → accum.pending_abs_x = Some(v)
   │  - EV_ABS ABS_Y → accum.pending_abs_y = Some(v)
   │  - EV_KEY       → enqueue Button packet (unchanged)
   │  - SYN_REPORT   → flush_frame: if pending_abs, normalize & enqueue AbsPos
   ▼
Scheduler (random delay per packet)
   ▼
uinput.rs emit_packet AbsPos { x, y }
   │  EV_ABS ABS_X, EV_ABS ABS_Y, EV_SYN SYN_REPORT
   ▼
Guest compositor (libinput treats virtual device as absolute pointer)
```

## Error handling

- `EVIOCGABS` failure at attach → log and skip the device (don't grab). VM tablets always answer this ioctl; failure means something is wrong with the device and we'd rather not grab it.
- `ABS_X` or `ABS_Y` reported max ≤ 0 → skip the device (would divide by zero or invert).
- Normalization clamps negative raw values to 0 and values > source_max to 32767 — the QEMU tablet doesn't send out-of-range values, but defensive.

## Testing

Unit tests (no kernel needed):

- `classify_vm_tablet`: synthetic bitmaps that look like QEMU USB Tablet — should classify as `VmTablet`.
- `classify_real_touchpad`: bitmaps with `ABS_MT_SLOT` set — should classify as touchpad and be skipped.
- `classify_keyboard`: no ABS → Keyboard. Classify as Keyboard.
- `classify_relmouse`: EV_KEY + EV_REL, no ABS → RelMouse.
- `abs_accumulator_flushes_on_syn`: feed ABS_X=1000, ABS_Y=2000, SYN_REPORT → one AbsPos packet, normalized against a known max.
- `abs_normalization_bounds`: raw=0 → 0; raw=max → 32767; raw=max/2 → 16383.

VM smoke test (reuses the existing VM harness):

1. Install the new deb in the libvirt guest.
2. Observe kloak grabs the QEMU USB Tablet (`virsh qemu-agent-command` + `ls /sys/class/input/…`).
3. Move the host cursor across the guest window; confirm cursor in the guest follows with the expected ~25 ms average delay.
4. Click with host mouse; confirm guest sees click.
5. Confirm a real touchpad on the host (outside the VM) is unaffected — N/A for VM-only testing but verify on bare metal separately if available.

## What this does *not* do

- Anonymize real-hardware touchpads. They pass to the compositor untouched. Documented as a known gap in the README; future work tracked as `A3` in the brainstorm.
- Handle ABS_MT_* events. Filtered at attach — they never reach translate.
- Handle ABS_PRESSURE, ABS_TILT_*, ABS_WHEEL on tablets. Out of scope; VM tablets don't emit these.

## Risk register

| Risk | Mitigation |
|---|---|
| GNOME Shell tags our virtual device as `ID_INPUT_TABLET` again | Set `INPUT_PROP_POINTER` via `UI_SET_PROPBIT`. Verify with `udevadm info /dev/input/eventN`. |
| Two VM tablets attached (extremely rare) with different ranges | Per-device max stored in `DeviceKind::VmTablet`; normalization happens before packet enqueue so the sink is always 0..32767. No collision. |
| MT-less real touchpad exists somewhere | Theoretically possible on very old hardware (pre-2010 Synaptics). Would be misclassified as VmTablet and partially broken. Documented caveat; user can disable per-device via config if it ever matters. |
| `EVIOCGABS` ioctl number differs per arch | It doesn't — `asm-generic/ioctl.h` is identical on amd64/aarch64, and `EVIOCGABS(abs) = _IOR('E', 0x40 + abs, struct input_absinfo)` is stable. Encode via our existing `ioc()` const-fn with `IOC_READ`. Unit-test the encoded value. |

## Implementation order

Captured in the writing-plans follow-up. Rough batches:

1. **Classification** — `DeviceKind` enum, `EVIOCGABS` helper, attach-time classification, unit tests.
2. **Packet plumbing** — `InputPacket::AbsPos`, FrameAccum fields, translate.rs arm, unit tests.
3. **Uinput sink** — EV_ABS bit, UI_ABS_SETUP, UI_SET_PROPBIT, emit_packet arm.
4. **Scheduler** — verify AbsPos ordering preservation (likely no change needed).
5. **VM smoke test** — install, drive cursor, confirm motion + click + escape-combo exit.
