//! uinput output backend — Rust port of [c/src/uinput.c].
//!
//! Opens `/dev/uinput`, declares every event code kloak may emit, and creates
//! a virtual device named "kloak" on `BUS_VIRTUAL`. Requires `CAP_SYS_ADMIN`
//! (kloak.service already grants it).
//!
//! This module is the sole concentrated-unsafe surface in the Rust crate. All
//! unsafe blocks are annotated with a SAFETY comment explaining why the call
//! is sound. The caller-visible API is fully safe.
//!
//! See §8 of the behavior matrix for the kernel contract.
//!
//! This module is Linux-only; `lib.rs` gates it with `cfg(target_os = "linux")`.

use std::ffi::c_int;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

use crate::event::InputPacket;

const UINPUT_DEV_PATH: &str = "/dev/uinput";

// ---------------------------------------------------------------------------
// Kernel ABI constants — mirrored from `<linux/input-event-codes.h>` and
// `<linux/uinput.h>`. These are stable kernel ABI.

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_MSC: u16 = 0x04;

const SYN_REPORT: u16 = 0x00;
const MSC_SCAN: u16 = 0x04;

/// Highest `KEY_*` / `BTN_*` code we advertise. Matches `KEY_MAX` in
/// `<linux/input-event-codes.h>` as of Linux 6.x. Like the C version, we
/// over-advertise deliberately so libinput can hand us any code from any
/// real device.
const KEY_MAX: u16 = 0x2ff;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;

const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

/// Digitizer/tablet-tool BTN_* range. `BTN_TOOL_PEN`=0x140 .. `BTN_TOOL_QUADTAP`=0x14f.
/// Advertising any of these alongside EV_ABS makes libinput treat the
/// device as a drawing tablet.
const BTN_TOOL_FIRST: u16 = 0x140;
const BTN_TOOL_LAST: u16 = 0x14f;

const BUS_VIRTUAL: u16 = 0x06;

const UINPUT_MAX_NAME_SIZE: usize = 80;

/// `INPUT_PROP_POINTER`: tells udev this is a mouse-style pointer, not a
/// drawing tablet. Without this, advertising EV_ABS + ABS_X/Y causes GNOME
/// Shell to tag the virtual device `ID_INPUT_TABLET` (the exact bug the
/// old C daemon hit). With the flag set, udev tags it `ID_INPUT_MOUSE`
/// and compositors map it to a pointer, which is what QEMU USB Tablet
/// and virtio-tablet need.
const INPUT_PROP_POINTER: u16 = 0x00;

/// Pointer-button codes we advertise on the absolute-pointer sink. Anything
/// outside this list — keyboard keys, tablet-tool buttons (`BTN_TOOL_*`),
/// joystick buttons — is intentionally omitted so libinput classifies the
/// device as a plain absolute mouse, not a keyboard or drawing tablet.
const POINTER_BUTTONS: &[u16] = &[
    0x110, // BTN_LEFT
    0x111, // BTN_RIGHT
    0x112, // BTN_MIDDLE
    0x113, // BTN_SIDE
    0x114, // BTN_EXTRA
    0x115, // BTN_FORWARD
    0x116, // BTN_BACK
    0x117, // BTN_TASK
];

/// Which kind of virtual device to open. Each kind produces a disjoint
/// capability set so libinput classifies them unambiguously:
/// - `Kbd` carries `EV_KEY` + `EV_REL` + `EV_MSC` — keyboard + relative mouse.
/// - `Pointer` carries `EV_KEY` (pointer buttons only) + `EV_ABS` with
///   `INPUT_PROP_POINTER` — absolute mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkKind {
    Kbd,
    Pointer,
}

/// One step in the uinput capability-setup sequence. A pure description so
/// `capability_plan` is testable without touching `/dev/uinput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapCall {
    SetEvBit(u16),
    SetKeyBit(u16),
    SetRelBit(u16),
    SetAbsBit(u16),
    SetMscBit(u16),
    SetPropBit(u16),
    AbsSetup { code: u16, min: i32, max: i32 },
}

/// Compute the ordered ioctl sequence for `kind`. Pure — returns the plan,
/// does not touch `/dev/uinput`. Kept separate so the capability set can be
/// asserted in unit tests.
fn capability_plan(kind: SinkKind) -> Vec<CapCall> {
    let mut plan = Vec::new();
    match kind {
        SinkKind::Kbd => {
            plan.push(CapCall::SetEvBit(EV_SYN));
            plan.push(CapCall::SetEvBit(EV_KEY));
            plan.push(CapCall::SetEvBit(EV_REL));
            plan.push(CapCall::SetEvBit(EV_MSC));
            for code in 1..=KEY_MAX {
                if (BTN_TOOL_FIRST..=BTN_TOOL_LAST).contains(&code) {
                    continue;
                }
                plan.push(CapCall::SetKeyBit(code));
            }
            plan.push(CapCall::SetRelBit(REL_X));
            plan.push(CapCall::SetRelBit(REL_Y));
            plan.push(CapCall::SetRelBit(REL_WHEEL));
            plan.push(CapCall::SetRelBit(REL_HWHEEL));
            plan.push(CapCall::SetRelBit(REL_WHEEL_HI_RES));
            plan.push(CapCall::SetRelBit(REL_HWHEEL_HI_RES));
            plan.push(CapCall::SetMscBit(MSC_SCAN));
        }
        SinkKind::Pointer => {
            plan.push(CapCall::SetEvBit(EV_SYN));
            plan.push(CapCall::SetEvBit(EV_KEY));
            plan.push(CapCall::SetEvBit(EV_ABS));
            plan.push(CapCall::SetPropBit(INPUT_PROP_POINTER));
            for &btn in POINTER_BUTTONS {
                plan.push(CapCall::SetKeyBit(btn));
            }
            plan.push(CapCall::SetAbsBit(ABS_X));
            plan.push(CapCall::SetAbsBit(ABS_Y));
            plan.push(CapCall::AbsSetup {
                code: ABS_X,
                min: 0,
                max: 32767,
            });
            plan.push(CapCall::AbsSetup {
                code: ABS_Y,
                min: 0,
                max: 32767,
            });
        }
    }
    plan
}

fn device_name(kind: SinkKind) -> &'static [u8] {
    match kind {
        SinkKind::Kbd => b"kloak-kbd",
        SinkKind::Pointer => b"kloak-pointer",
    }
}

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

#[repr(C)]
#[derive(Copy, Clone)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; UINPUT_MAX_NAME_SIZE],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputEvent {
    tv_sec: libc::time_t,
    tv_usec: libc::suseconds_t,
    type_: u16,
    code: u16,
    value: i32,
}

// ---------------------------------------------------------------------------
// ioctl request encoding. `asm-generic/ioctl.h` is the authoritative source
// and is identical on x86, x86_64, and aarch64 — the three targets kloak
// ships to.

const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;

const fn ioc(dir: u32, ty: u8, nr: u8, size: u32) -> u32 {
    (dir << 30) | ((ty as u32) << 8) | (nr as u32) | (size << 16)
}

const fn io_none(ty: u8, nr: u8) -> u32 {
    ioc(IOC_NONE, ty, nr, 0)
}
const fn iow(ty: u8, nr: u8, size: u32) -> u32 {
    ioc(IOC_WRITE, ty, nr, size)
}

const UINPUT_IOCTL_BASE: u8 = b'U';
const UI_DEV_CREATE: u32 = io_none(UINPUT_IOCTL_BASE, 1);
const UI_DEV_DESTROY: u32 = io_none(UINPUT_IOCTL_BASE, 2);
const UI_DEV_SETUP: u32 = iow(UINPUT_IOCTL_BASE, 3, size_of::<UinputSetup>() as u32);
const UI_SET_EVBIT: u32 = iow(UINPUT_IOCTL_BASE, 100, size_of::<c_int>() as u32);
const UI_SET_KEYBIT: u32 = iow(UINPUT_IOCTL_BASE, 101, size_of::<c_int>() as u32);
const UI_SET_RELBIT: u32 = iow(UINPUT_IOCTL_BASE, 102, size_of::<c_int>() as u32);
const UI_SET_ABSBIT: u32 = iow(UINPUT_IOCTL_BASE, 103, size_of::<c_int>() as u32);
const UI_SET_MSCBIT: u32 = iow(UINPUT_IOCTL_BASE, 104, size_of::<c_int>() as u32);
const UI_SET_PROPBIT: u32 = iow(UINPUT_IOCTL_BASE, 110, size_of::<c_int>() as u32);
const UI_ABS_SETUP: u32 = iow(UINPUT_IOCTL_BASE, 4, size_of::<UinputAbsSetup>() as u32);

// ---------------------------------------------------------------------------
// Public API

/// Owning handle to an open uinput device. Drops the virtual device and
/// closes the fd automatically.
#[derive(Debug)]
pub struct UInput {
    file: File,
}

impl UInput {
    /// Open `/dev/uinput` and create a virtual device of the given kind.
    /// Capability set is computed by [`capability_plan`] — kept separate so
    /// it is testable without touching the kernel.
    ///
    /// Fails with `io::Error` (errno-backed) if any step fails; partial
    /// state is cleaned up by `File`'s Drop via the `?` early-return.
    pub fn open_with(kind: SinkKind) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(UINPUT_DEV_PATH)?;
        let fd = file.as_raw_fd();

        for call in capability_plan(kind) {
            match call {
                CapCall::SetEvBit(code) => set_evbit(fd, code)?,
                CapCall::SetKeyBit(code) => set_keybit(fd, code)?,
                CapCall::SetRelBit(code) => set_relbit(fd, code)?,
                CapCall::SetAbsBit(code) => set_absbit(fd, code)?,
                CapCall::SetMscBit(code) => set_mscbit(fd, code)?,
                CapCall::SetPropBit(code) => ioctl_int(fd, UI_SET_PROPBIT, c_int::from(code))?,
                CapCall::AbsSetup { code, min, max } => abs_setup(fd, code, min, max)?,
            }
        }

        dev_setup(fd, device_name(kind))?;
        dev_create(fd)?;

        Ok(Self { file })
    }

    /// Open the keyboard / relative-mouse sink (`kloak-kbd`).
    pub fn open_kbd() -> io::Result<Self> {
        Self::open_with(SinkKind::Kbd)
    }

    /// Open the absolute-pointer sink (`kloak-pointer`).
    pub fn open_pointer() -> io::Result<Self> {
        Self::open_with(SinkKind::Pointer)
    }

    /// Raw evdev record emitter. Thin wrapper around `write(2)`.
    pub fn emit(&self, type_: u16, code: u16, value: i32) -> io::Result<()> {
        let ev = InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            type_,
            code,
            value,
        };
        // SAFETY: `ev` is a fully initialized `#[repr(C)]` POD with no
        // padding the kernel cares about (input_event's layout is stable
        // kernel ABI). We form a &[u8] of exactly size_of::<InputEvent>()
        // bytes from its address. The slice is not aliased mutably and is
        // only read by `write(2)` for the duration of the call.
        let n = unsafe {
            libc::write(
                self.file.as_raw_fd(),
                (&ev as *const InputEvent).cast(),
                size_of::<InputEvent>(),
            )
        };
        if n == size_of::<InputEvent>() as isize {
            Ok(())
        } else if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "partial uinput write",
            ))
        }
    }

    /// Emit `EV_SYN/SYN_REPORT/0` — call once per logical event group.
    pub fn syn(&self) -> io::Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }

    /// Emit one [`InputPacket`] as the matching evdev record group
    /// terminated by `SYN_REPORT`. Matches the C daemon's group layout.
    pub fn emit_packet(&self, packet: InputPacket) -> io::Result<()> {
        match packet {
            InputPacket::Key { code, pressed } | InputPacket::Button { code, pressed } => {
                let code_u16 =
                    u16::try_from(code).map_err(|_| io::Error::other("key code out of range"))?;
                self.emit(EV_KEY, code_u16, if pressed { 1 } else { 0 })?;
            }
            InputPacket::Motion { dx, dy } => {
                self.emit(EV_REL, REL_X, dx)?;
                self.emit(EV_REL, REL_Y, dy)?;
            }
            InputPacket::Scroll { vert, horiz } => {
                if vert != 0 {
                    self.emit(EV_REL, REL_WHEEL, vert)?;
                }
                if horiz != 0 {
                    self.emit(EV_REL, REL_HWHEEL, horiz)?;
                }
            }
            InputPacket::AbsPos { x, y } => {
                self.emit(EV_ABS, ABS_X, x)?;
                self.emit(EV_ABS, ABS_Y, y)?;
            }
        }
        self.syn()
    }
}

impl Drop for UInput {
    fn drop(&mut self) {
        // SAFETY: `self.file` owns a valid fd for the lifetime of &mut self.
        // UI_DEV_DESTROY takes no argument and has no side effect beyond
        // tearing down the virtual device. Any error is swallowed because
        // Drop cannot return one — matches C's `uinput_close()` semantics.
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY as _);
        }
    }
}

// ---------------------------------------------------------------------------
// Internal ioctl helpers

fn ioctl_int(fd: RawFd, req: u32, val: c_int) -> io::Result<()> {
    // SAFETY: The UI_SET_*BIT ioctls take a single `int` passed by value.
    // We pass a plain C int; the kernel reads the value from the syscall
    // arg register. `fd` is borrowed from File which outlives this call.
    let rc = unsafe { libc::ioctl(fd, req as _, val) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_evbit(fd: RawFd, code: u16) -> io::Result<()> {
    ioctl_int(fd, UI_SET_EVBIT, c_int::from(code))
}
fn set_keybit(fd: RawFd, code: u16) -> io::Result<()> {
    ioctl_int(fd, UI_SET_KEYBIT, c_int::from(code))
}
fn set_relbit(fd: RawFd, code: u16) -> io::Result<()> {
    ioctl_int(fd, UI_SET_RELBIT, c_int::from(code))
}
fn set_mscbit(fd: RawFd, code: u16) -> io::Result<()> {
    ioctl_int(fd, UI_SET_MSCBIT, c_int::from(code))
}

fn set_absbit(fd: RawFd, code: u16) -> io::Result<()> {
    ioctl_int(fd, UI_SET_ABSBIT, c_int::from(code))
}

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
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn dev_setup(fd: RawFd, label: &[u8]) -> io::Result<()> {
    let mut name = [0u8; UINPUT_MAX_NAME_SIZE];
    let len = label.len().min(UINPUT_MAX_NAME_SIZE - 1);
    name[..len].copy_from_slice(&label[..len]);
    let setup = UinputSetup {
        id: InputId {
            bustype: BUS_VIRTUAL,
            vendor: 0x6B6C,  // "kl"
            product: 0x6F61, // "oa"
            version: 1,
        },
        name,
        ff_effects_max: 0,
    };
    // SAFETY: `setup` is fully initialized; UI_DEV_SETUP reads exactly
    // size_of::<UinputSetup>() bytes from the pointer for the call.
    let rc = unsafe { libc::ioctl(fd, UI_DEV_SETUP as _, &setup as *const UinputSetup) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn dev_create(fd: RawFd) -> io::Result<()> {
    // SAFETY: UI_DEV_CREATE is _IO (no argument). The kernel ignores the
    // third ioctl arg; we pass nothing. `fd` is a live uinput fd.
    let rc = unsafe { libc::ioctl(fd, UI_DEV_CREATE as _) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_kind_kbd_advertises_rel_not_abs() {
        let plan = capability_plan(SinkKind::Kbd);
        assert!(plan.contains(&CapCall::SetEvBit(EV_REL)));
        assert!(!plan.contains(&CapCall::SetEvBit(EV_ABS)));
        assert!(plan.iter().any(|c| matches!(c, CapCall::SetRelBit(_))));
        assert!(!plan.iter().any(|c| matches!(c, CapCall::SetAbsBit(_))));
        // No INPUT_PROP_POINTER on the kbd sink — it's a keyboard + relmouse.
        assert!(!plan.iter().any(|c| matches!(c, CapCall::SetPropBit(_))));
    }

    #[test]
    fn sink_kind_pointer_advertises_abs_not_rel() {
        let plan = capability_plan(SinkKind::Pointer);
        assert!(plan.contains(&CapCall::SetEvBit(EV_ABS)));
        assert!(!plan.contains(&CapCall::SetEvBit(EV_REL)));
        assert!(plan.contains(&CapCall::SetAbsBit(ABS_X)));
        assert!(plan.contains(&CapCall::SetAbsBit(ABS_Y)));
        assert!(!plan.iter().any(|c| matches!(c, CapCall::SetRelBit(_))));
        assert!(plan.contains(&CapCall::SetPropBit(INPUT_PROP_POINTER)));
        assert!(plan.contains(&CapCall::AbsSetup {
            code: ABS_X,
            min: 0,
            max: 32767,
        }));
    }

    #[test]
    fn sink_kind_pointer_advertises_only_mouse_buttons() {
        let plan = capability_plan(SinkKind::Pointer);
        // BTN_LEFT present.
        assert!(plan.contains(&CapCall::SetKeyBit(0x110)));
        // A keyboard key (KEY_A = 0x1e) must NOT be present on the pointer.
        assert!(!plan.contains(&CapCall::SetKeyBit(0x1e)));
        // BTN_TOOL_PEN (0x140) must NOT be present — would trigger libinput
        // tablet classification and the "missing tablet resolution" reject.
        assert!(!plan.contains(&CapCall::SetKeyBit(0x140)));
        assert!(!plan.contains(&CapCall::SetKeyBit(0x14b))); // BTN_STYLUS
    }

    #[test]
    fn sink_kind_kbd_advertises_keyboard_and_mouse_buttons() {
        let plan = capability_plan(SinkKind::Kbd);
        assert!(plan.contains(&CapCall::SetKeyBit(0x1e))); // KEY_A
        assert!(plan.contains(&CapCall::SetKeyBit(0x110))); // BTN_LEFT
    }

    #[test]
    fn sink_kind_kbd_skips_tablet_tool_range() {
        let plan = capability_plan(SinkKind::Kbd);
        for code in BTN_TOOL_FIRST..=BTN_TOOL_LAST {
            assert!(
                !plan.contains(&CapCall::SetKeyBit(code)),
                "BTN_TOOL_* code 0x{:x} leaked into kbd plan",
                code
            );
        }
    }

    #[test]
    fn sink_kind_names_are_distinct() {
        assert_eq!(device_name(SinkKind::Kbd), b"kloak-kbd");
        assert_eq!(device_name(SinkKind::Pointer), b"kloak-pointer");
    }

    #[test]
    fn ioctl_numbers_match_kernel_abi() {
        // Spot-check that our hand-rolled _IO / _IOW match the values the
        // kernel's `<linux/uinput.h>` would expand to on amd64/aarch64.
        // These are computed once by the kernel and shipped verbatim.
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(UI_SET_EVBIT, 0x40045564);
        assert_eq!(UI_SET_KEYBIT, 0x40045565);
        assert_eq!(UI_SET_RELBIT, 0x40045566);
        assert_eq!(UI_SET_ABSBIT, 0x40045567);
        assert_eq!(UI_SET_MSCBIT, 0x40045568);
        assert_eq!(UI_SET_PROPBIT, 0x4004556e);
        // UI_ABS_SETUP = _IOW('U', 4, struct uinput_abs_setup(28 bytes)).
        assert_eq!(UI_ABS_SETUP, 0x401c5504);
    }

    #[test]
    fn uinput_abs_setup_layout() {
        // struct uinput_abs_setup = __u16 code + 2 bytes pad + input_absinfo(24) = 28.
        assert_eq!(size_of::<UinputAbsSetup>(), 28);
    }

    #[test]
    fn uinput_setup_layout() {
        // Kernel expects: input_id (8) + char[80] + __u32 (4) = 92 bytes.
        assert_eq!(size_of::<InputId>(), 8);
        assert_eq!(size_of::<UinputSetup>(), 8 + 80 + 4);
    }
}
