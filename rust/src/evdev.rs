//! Per-device evdev reader with EVIOCGRAB + capability classification.
//!
//! Replaces the prior libinput-based path. Each tracked device owns a
//! `/dev/input/eventN` handle, opened `O_NONBLOCK|O_CLOEXEC` and exclusively
//! grabbed via `EVIOCGRAB`. The reader yields raw `(type, code, value)`
//! tuples from the kernel evdev protocol; translation into `InputPacket`s
//! happens in `translate.rs`.
//!
//! This module is the sole evdev-ABI surface in the crate.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::mem::{size_of, MaybeUninit};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use crate::event::Sink;
use crate::translate::FrameAccum;

// ---------------------------------------------------------------------------
// Kernel evdev ABI — `<linux/input.h>` and `<linux/input-event-codes.h>`.

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;

pub const SYN_REPORT: u16 = 0x00;

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
    // EV_ABS present. Real laptop touchpads advertise ABS_MT_SLOT (Protocol B);
    // leave those to the compositor — kloak can't faithfully mirror MT gestures
    // through a non-MT uinput sink. VM-emulated tablets (QEMU USB Tablet,
    // virtio-tablet, spice vdagent tablet) have no ABS_MT_SLOT and route to
    // the dedicated `kloak-pointer` sink so their button timing is anonymized
    // without ABS events fighting libinput's REL+ABS classification.
    if has_bit(abs_bits, ABS_MT_SLOT as usize) {
        DeviceClass::Skip
    } else {
        DeviceClass::VmTablet
    }
}

/// `struct input_event` exactly as the kernel writes it. Layout is stable
/// evdev ABI on every Linux architecture kloak targets (amd64/aarch64 use
/// 16-byte `struct timeval`, so total size is 24 bytes).
#[repr(C)]
#[derive(Copy, Clone)]
struct InputEvent {
    tv_sec: libc::time_t,
    tv_usec: libc::suseconds_t,
    type_: u16,
    code: u16,
    value: i32,
}

// ---------------------------------------------------------------------------
// ioctl encoding — `asm-generic/ioctl.h` (identical on amd64/aarch64).

const IOC_READ: u32 = 2;
const IOC_WRITE: u32 = 1;

const fn ioc(dir: u32, ty: u8, nr: u16, size: u32) -> u32 {
    (dir << 30) | ((ty as u32) << 8) | (nr as u32) | (size << 16)
}

/// `EVIOCGRAB = _IOW('E', 0x90, int)` — exclusive-grab the device.
const EVIOCGRAB: libc::c_ulong = ioc(IOC_WRITE, b'E', 0x90, size_of::<libc::c_int>() as u32) as _;

/// `EVIOCGBIT(ev, len) = _IOC(_IOC_READ, 'E', 0x20 + ev, len)`.
const fn eviocgbit(ev: u8, len: u32) -> libc::c_ulong {
    ioc(IOC_READ, b'E', 0x20u16 + ev as u16, len) as libc::c_ulong
}

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
    ioc(
        IOC_READ,
        b'E',
        0x40u16 + abs as u16,
        size_of::<InputAbsinfo>() as u32,
    ) as libc::c_ulong
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

// ---------------------------------------------------------------------------
// Public API

/// One grabbed `/dev/input/eventN`. Owns the fd and the per-device
/// SYN-frame accumulator consumed by `translate::flush_frame`. The
/// accumulator's `has_hi_res_*` flags are populated from EVIOCGBIT at
/// open time so translation doesn't need to re-query every event.
#[derive(Debug)]
pub struct EvdevDevice {
    file: File,
    name: String,
    pub frame: FrameAccum,
}

impl EvdevDevice {
    /// File descriptor for polling.
    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// Non-blocking drain of all pending events into `out`. Pushes one
    /// `(type, code, value)` tuple per kernel record; returns when `read`
    /// would block. `out` is not cleared — callers typically reuse a
    /// shared buffer and drain it after this call.
    pub fn drain_into(&mut self, out: &mut Vec<(u16, u16, i32)>) {
        loop {
            let mut ev: MaybeUninit<InputEvent> = MaybeUninit::uninit();
            // SAFETY: `ev` has the exact layout and size the kernel writes.
            // Reading into a `MaybeUninit<InputEvent>` via `&mut [u8]` is
            // sound because `InputEvent` is `#[repr(C)]` POD with no niche.
            let buf = unsafe {
                std::slice::from_raw_parts_mut(
                    ev.as_mut_ptr().cast::<u8>(),
                    size_of::<InputEvent>(),
                )
            };
            match self.file.read(buf) {
                Ok(0) => return,
                Ok(n) if n == size_of::<InputEvent>() => {
                    // SAFETY: `read` returned exactly `InputEvent`-sized
                    // bytes, which the kernel promises is a valid record.
                    let ev = unsafe { ev.assume_init() };
                    out.push((ev.type_, ev.code, ev.value));
                }
                Ok(_) => {
                    eprintln!(
                        "WARNING: short evdev read on {} (dropping partial record)",
                        self.name
                    );
                    return;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.raw_os_error() == Some(libc::ENODEV) => {
                    // Device was unplugged mid-read; caller will detach via
                    // inotify shortly. Nothing more to drain.
                    return;
                }
                Err(e) => {
                    eprintln!(
                        "WARNING: evdev read on {} failed: {} (dropping events)",
                        self.name, e
                    );
                    return;
                }
            }
        }
    }

    fn open(name: &str, suppress_vm_tablet: bool) -> io::Result<Option<Self>> {
        let path: PathBuf = format!("/dev/input/{name}").into();
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)?;
        let fd = file.as_raw_fd();

        let ev_bits = query_bits::<4>(fd, 0)?;
        let has_rel = has_bit(&ev_bits, EV_REL as usize);
        let has_abs = has_bit(&ev_bits, EV_ABS as usize);

        // Touchpad / tablet / touchscreen classification. VM tablets
        // (QEMU USB Tablet, virtio-tablet) have EV_ABS without ABS_MT_SLOT
        // and we grab them; real laptop touchpads advertise ABS_MT_SLOT and
        // we leave them alone so their compositor-side gestures keep working.
        let abs_bits: [u8; 8] = if has_abs {
            query_bits::<8>(fd, EV_ABS as u8)?
        } else {
            [0u8; 8]
        };

        let (abs_x_max, abs_y_max) = match classify(&ev_bits, &abs_bits) {
            DeviceClass::Skip => return Ok(None),
            DeviceClass::KeyOrRel => (None, None),
            DeviceClass::VmTablet => {
                if suppress_vm_tablet {
                    // A VM tablet was already attached (e.g. QEMU USB Tablet)
                    // and a second duplicate pointer source showed up (e.g.
                    // spice vdagent tablet creating its own /dev/input/eventN).
                    // Grab the device exclusively so the compositor doesn't
                    // read it directly — but leave `abs_x_max` = None so the
                    // translate layer silently drops its ABS events. Without
                    // this the two streams interleave in the scheduler and
                    // the cursor jumps between sources.
                    (None, None)
                } else {
                    let x_info = query_absinfo(fd, ABS_X as u8)?;
                    let y_info = query_absinfo(fd, ABS_Y as u8)?;
                    if x_info.maximum <= 0 || y_info.maximum <= 0 {
                        // Defensive: a zero/negative range would divide by zero
                        // in the translate-layer normalization. Skip quietly.
                        return Ok(None);
                    }
                    (Some(x_info.maximum), Some(y_info.maximum))
                }
            }
        };

        let (has_hi_res_vwheel, has_hi_res_hwheel) = if has_rel {
            let rel_bits = query_bits::<2>(fd, EV_REL as u8)?;
            const REL_WHEEL_HI_RES: usize = 0x0b;
            const REL_HWHEEL_HI_RES: usize = 0x0c;
            (
                has_bit(&rel_bits, REL_WHEEL_HI_RES),
                has_bit(&rel_bits, REL_HWHEEL_HI_RES),
            )
        } else {
            (false, false)
        };

        // EVIOCGRAB: exclusive grab. If this fails (already grabbed by
        // another process), we bail loudly — the C daemon did the same.
        let one: libc::c_int = 1;
        // SAFETY: `fd` is a live file descriptor held by `file`; EVIOCGRAB
        // reads a single int via the third arg.
        let rc = unsafe { libc::ioctl(fd, EVIOCGRAB, &one as *const libc::c_int) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            eprintln!(
                "FATAL ERROR: Could not grab evdev device '{}'!",
                path.display()
            );
            return Err(err);
        }

        let sink = match classify(&ev_bits, &abs_bits) {
            DeviceClass::VmTablet => Sink::Pointer,
            _ => Sink::Kbd,
        };
        let frame = FrameAccum {
            has_hi_res_vwheel,
            has_hi_res_hwheel,
            abs_x_max,
            abs_y_max,
            sink,
            ..FrameAccum::default()
        };
        Ok(Some(Self {
            file,
            name: name.to_string(),
            frame,
        }))
    }
}

fn has_bit(bits: &[u8], n: usize) -> bool {
    let byte = n / 8;
    let mask = 1u8 << (n % 8);
    bits.get(byte).is_some_and(|b| b & mask != 0)
}

fn query_bits<const N: usize>(fd: RawFd, ev: u8) -> io::Result<[u8; N]> {
    let mut buf = [0u8; N];
    // SAFETY: EVIOCGBIT writes up to `N` bytes into `buf`; we encode that
    // length in the ioctl number. `fd` is a live evdev fd.
    let rc = unsafe { libc::ioctl(fd, eviocgbit(ev, N as u32), buf.as_mut_ptr()) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(buf)
    }
}

/// Multi-device manager. Replaces the libinput path-backend context.
#[derive(Debug, Default)]
pub struct EvdevCtx {
    devices: HashMap<String, EvdevDevice>,
}

impl EvdevCtx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a device by short name (e.g. `"event3"`).
    ///
    /// Silently skips:
    /// - Devices we cannot handle (no EV_KEY, or real touchpads /
    ///   touchscreens / tablets advertising ABS_MT_SLOT).
    /// - Open/grab errors other than "already tracked".
    /// - Already-tracked names, after first detaching (hot-unplug race).
    pub fn attach(&mut self, name: &str) {
        if self.devices.contains_key(name) {
            self.detach(name);
        }
        let suppress_vm_tablet = self.has_vm_tablet();
        match EvdevDevice::open(name, suppress_vm_tablet) {
            Ok(Some(dev)) => {
                self.devices.insert(name.to_string(), dev);
            }
            Ok(None) => {
                // Device doesn't match our filter (no EV_KEY, or real
                // touchpad/touchscreen with ABS_MT_SLOT).
            }
            Err(e) => {
                eprintln!("WARNING: could not open /dev/input/{}: {}", name, e);
            }
        }
    }

    fn has_vm_tablet(&self) -> bool {
        self.devices.values().any(|d| d.frame.abs_x_max.is_some())
    }

    /// Detach by short name. No-op if not tracked.
    pub fn detach(&mut self, name: &str) {
        self.devices.remove(name);
        // File drop closes the fd; the kernel releases the EVIOCGRAB.
    }

    /// Iterate all tracked devices, letting the caller read events. The
    /// closure gets a `&mut EvdevDevice` so it can drain and update the
    /// device-local SYN-frame accumulator.
    pub fn devices_mut(&mut self) -> impl Iterator<Item = &mut EvdevDevice> {
        self.devices.values_mut()
    }

    /// Look up a device by short name. Used by the poll loop to service
    /// only the fds that were actually marked readable.
    pub fn device_mut(&mut self, name: &str) -> Option<&mut EvdevDevice> {
        self.devices.get_mut(name)
    }

    /// Snapshot of tracked device names in insertion-independent order.
    pub fn names(&self) -> Vec<String> {
        self.devices.keys().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evdev_ioctl_numbers_match_kernel_abi() {
        // EVIOCGRAB = 0x40044590 on amd64/aarch64. Spot-check our encoder.
        assert_eq!(EVIOCGRAB, 0x4004_4590);
        // EVIOCGBIT(0, 4) = 0x80044520 — 4 bytes read of event-type bitmap.
        assert_eq!(eviocgbit(0, 4), 0x8004_4520);
        // EVIOCGBIT(EV_REL, 2) = 0x80024522.
        assert_eq!(eviocgbit(EV_REL as u8, 2), 0x8002_4522);
    }

    #[test]
    fn eviocgabs_number_matches_kernel_abi() {
        // EVIOCGABS(ABS_X) = _IOR('E', 0x40, struct input_absinfo(24 bytes)).
        assert_eq!(eviocgabs(ABS_X as u8), 0x8018_4540);
        assert_eq!(eviocgabs(ABS_Y as u8), 0x8018_4541);
    }

    #[test]
    fn input_absinfo_size_is_24_bytes() {
        assert_eq!(size_of::<InputAbsinfo>(), 24);
    }

    #[test]
    fn has_bit_reads_little_endian_bytes() {
        // Byte 0 bit 1 set = EV_KEY.
        let bits = [0b0000_0010u8, 0, 0, 0];
        assert!(has_bit(&bits, 1));
        assert!(!has_bit(&bits, 0));
        assert!(!has_bit(&bits, 2));
        // Byte 1 bit 3 set = bit #11 overall.
        let bits = [0, 0b0000_1000u8, 0, 0];
        assert!(has_bit(&bits, 11));
        assert!(!has_bit(&bits, 10));
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

    #[test]
    fn classify_vm_tablet_is_vm_tablet() {
        // VM-emulated tablets (EV_ABS without ABS_MT_SLOT) route to the
        // dedicated kloak-pointer sink. Libinput sees a pure absolute
        // pointer (no EV_REL on that sink) so ABS events pass through
        // cleanly.
        let ev_bits = make_ev_bits(&[EV_KEY, EV_ABS]);
        let abs_bits = make_abs_bits(&[ABS_X, ABS_Y]);
        assert_eq!(classify(&ev_bits, &abs_bits), DeviceClass::VmTablet);
    }

    #[test]
    fn classify_real_touchpad_has_mt_slot() {
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
        let ev_bits = make_ev_bits(&[EV_ABS]);
        let abs_bits = make_abs_bits(&[ABS_X, ABS_Y]);
        assert_eq!(classify(&ev_bits, &abs_bits), DeviceClass::Skip);
    }

    #[test]
    fn input_event_size_is_24_bytes_on_64_bit() {
        // amd64 and aarch64 both use 64-bit time_t / suseconds_t;
        // `input_event` is 8 + 8 + 2 + 2 + 4 = 24 bytes.
        assert_eq!(size_of::<InputEvent>(), 24);
    }
}
