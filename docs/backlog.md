# kloak-ubuntu backlog

Parked ideas and enhancements that aren't urgent. Promote to `docs/plans/`
when ready to work on.

---

## Touchpad MT protocol B passthrough

**Why it's not done yet:** kloak currently skips any device advertising
`ABS_MT_SLOT` (see `classify()` in `rust/src/evdev.rs`). Real laptop
touchpads use multitouch protocol B — per-slot `ABS_MT_POSITION_X/Y`,
`ABS_MT_TRACKING_ID`, `ABS_MT_PRESSURE`, etc. — and a naive non-MT
uinput sink drops tap-to-click, two-finger scroll, pinch-zoom, and
palm-rejection.

**Consequence on bare metal:** keyboards, external mice, and TrackPoints
are timing-anonymized; the laptop touchpad is not (its button-tap
timing leaks).

**Sketch of the work:**

1. Extend `classify()` to recognize `Touchpad` as a class distinct from
   `VmTablet` and `Skip`. Trigger: `EV_KEY` + `EV_ABS` + `ABS_MT_SLOT`.
2. In `uinput::capability_plan`, add a `SinkKind::Touchpad` (or fold
   into `Pointer`) that advertises the full MT axis set: `ABS_MT_SLOT`,
   `ABS_MT_TRACKING_ID`, `ABS_MT_POSITION_X/Y`, `ABS_MT_PRESSURE`,
   `ABS_MT_TOUCH_MAJOR/MINOR`, plus `INPUT_PROP_POINTER` and the source
   device's reported ranges (query via `EVIOCGABS`).
3. Copy axis ranges from the source touchpad verbatim — any mismatch
   breaks libinput's gesture detector.
4. Pass MT slot frames through unchanged (no jitter — anonymizing
   per-sample timing of a finger stroke would feel like the cursor is
   skipping).
5. Jitter only `BTN_TOUCH`, `BTN_TOOL_FINGER`, `BTN_LEFT` via the
   existing `enqueue_button` path.
6. VM smoke-test equivalent doesn't apply; needs a laptop-hardware
   verification run (two-finger scroll, pinch, palm rejection, drag).

**Risk:** HIGH. libinput's touchpad detection is fragile and sensitive
to which axis bits and props the device advertises. A uinput sink that
looks *almost* like a real touchpad can cause libinput to silently
disable features, so the verification bar is "every gesture still
works," not "cursor moves."

**Deferred because:** external-mouse workaround is fine for the
current maintainer's setup; the full MT passthrough is a multi-day
investment that needs real laptop hardware to validate against.
