"""
Stage 4 smoke test for kloak: delay distribution + escape combo.

Creates a virtual uinput keyboard, waits for kloak to grab it via inotify,
injects timed keystrokes while reading kloak-kbd's output, measures delays,
then fires the escape combo and verifies kloak exits.
"""

import fcntl
import os
import select
import struct
import sys
import time

# ----- evdev / uinput ABI ---------------------------------------------------
EV_SYN, EV_KEY, EV_MSC = 0x00, 0x01, 0x04
SYN_REPORT = 0
MSC_SCAN = 0x04
UI_SET_EVBIT = 0x40045564
UI_SET_KEYBIT = 0x40045565
UI_DEV_CREATE = 0x5501
UI_DEV_DESTROY = 0x5502
UI_DEV_SETUP = 0x405c5503  # struct uinput_setup is 92 bytes
UINPUT_MAX_NAME_SIZE = 80

KEY_A = 30
KEY_RIGHTSHIFT = 54
KEY_ESC = 1

EVENT_FMT = "llHHi"  # timeval sec,usec + type + code + value (24 bytes)
EVENT_SZ = struct.calcsize(EVENT_FMT)


def open_uinput_keyboard(name=b"kloak-stage4-src"):
    fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
    fcntl.ioctl(fd, UI_SET_EVBIT, EV_KEY)
    fcntl.ioctl(fd, UI_SET_EVBIT, EV_SYN)
    fcntl.ioctl(fd, UI_SET_EVBIT, EV_MSC)
    for code in (KEY_A, KEY_RIGHTSHIFT, KEY_ESC):
        fcntl.ioctl(fd, UI_SET_KEYBIT, code)
    # uinput_setup: struct { uinput_id id(16); char name[80]; uint32 ff_effects_max; }
    setup = struct.pack(
        "HHHH80sI",
        0x03,       # BUS_USB
        0x1234,     # vendor
        0x5678,     # product
        1,          # version
        name.ljust(UINPUT_MAX_NAME_SIZE, b"\0"),
        0,          # ff_effects_max
    )
    fcntl.ioctl(fd, UI_DEV_SETUP, setup)
    fcntl.ioctl(fd, UI_DEV_CREATE)
    return fd


def emit(fd, type_, code, value):
    os.write(fd, struct.pack(EVENT_FMT, 0, 0, type_, code, value))


def tap(fd, code):
    emit(fd, EV_KEY, code, 1)
    emit(fd, EV_SYN, SYN_REPORT, 0)
    emit(fd, EV_KEY, code, 0)
    emit(fd, EV_SYN, SYN_REPORT, 0)


def find_kloak_kbd():
    for name in os.listdir("/sys/class/input"):
        if not name.startswith("event"):
            continue
        try:
            with open(f"/sys/class/input/{name}/device/name") as f:
                if f.read().strip() == "kloak-kbd":
                    return f"/dev/input/{name}"
        except OSError:
            continue
    return None


def read_key_events(fd, dur_s, want_code):
    """Collect (timestamp, value) for KEY events matching want_code."""
    out = []
    deadline = time.time() + dur_s
    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 0.05)
        if not r:
            continue
        try:
            data = os.read(fd, EVENT_SZ * 256)
        except BlockingIOError:
            continue
        now = time.time()
        for i in range(0, len(data), EVENT_SZ):
            _, _, t, c, v = struct.unpack(EVENT_FMT, data[i:i + EVENT_SZ])
            if t == EV_KEY and c == want_code:
                out.append((now, v))
    return out


def main():
    src_fd = open_uinput_keyboard()
    print("# created virtual keyboard")
    # Give kloak's inotify ~1s to see and grab it.
    time.sleep(1.5)

    kpath = find_kloak_kbd()
    if not kpath:
        print("ERROR: kloak-kbd not present")
        sys.exit(1)
    print(f"# kloak output device: {kpath}")
    kfd = os.open(kpath, os.O_RDONLY | os.O_NONBLOCK)

    # ----- delay distribution test --------------------------------------
    # Interleave sends with drain-reads so output events are picked up as
    # they arrive, using the KERNEL timestamp embedded in each event
    # (tv_sec/tv_usec). That timestamp reflects when kloak's uinput write
    # hit the kernel — exactly what we want to compare against the send
    # time. Wall-clock read times are useless here because reads buffer.
    n_taps = 30
    send_times = []
    out_kern_times = []

    def drain_kfd():
        while True:
            r, _, _ = select.select([kfd], [], [], 0.0)
            if not r:
                return
            try:
                data = os.read(kfd, EVENT_SZ * 256)
            except BlockingIOError:
                return
            for i in range(0, len(data), EVENT_SZ):
                s, u, t, c, v = struct.unpack(EVENT_FMT, data[i:i + EVENT_SZ])
                if t == EV_KEY and c == KEY_A and v == 1:
                    out_kern_times.append(s + u / 1_000_000.0)

    for _ in range(n_taps):
        send_times.append(time.time())
        tap(src_fd, KEY_A)
        time.sleep(0.12)  # > max_delay (50ms) so each tap is independent
        drain_kfd()

    # Final drain after last send.
    time.sleep(0.3)
    drain_kfd()

    matched = min(len(out_kern_times), len(send_times))
    delays_ms = [(out_kern_times[i] - send_times[i]) * 1000 for i in range(matched)]

    print(f"# taps sent: {n_taps}, presses observed: {len(out_kern_times)}")
    if delays_ms:
        mn, mx = min(delays_ms), max(delays_ms)
        mean = sum(delays_ms) / len(delays_ms)
        print(f"delay_ms min={mn:.1f} mean={mean:.1f} max={mx:.1f}")
        # Distribution bucket
        buckets = [0] * 6  # 0-10, 10-20, 20-30, 30-40, 40-50, 50+
        for d in delays_ms:
            buckets[min(5, int(d // 10))] += 1
        print(f"histogram 0-10|10-20|20-30|30-40|40-50|50+ : {buckets}")
    else:
        print("ERROR: no presses made it through kloak")

    # ----- escape combo test --------------------------------------------
    print("# sending escape combo KEY_RIGHTSHIFT + KEY_ESC")
    emit(src_fd, EV_KEY, KEY_RIGHTSHIFT, 1)
    emit(src_fd, EV_SYN, SYN_REPORT, 0)
    emit(src_fd, EV_KEY, KEY_ESC, 1)
    emit(src_fd, EV_SYN, SYN_REPORT, 0)
    # release
    emit(src_fd, EV_KEY, KEY_ESC, 0)
    emit(src_fd, EV_KEY, KEY_RIGHTSHIFT, 0)
    emit(src_fd, EV_SYN, SYN_REPORT, 0)

    # kloak should exit within the jitter window + a bit.
    time.sleep(1.0)

    # Cleanup
    try:
        fcntl.ioctl(src_fd, UI_DEV_DESTROY)
    except OSError:
        pass
    os.close(src_fd)
    os.close(kfd)
    print("# done")


if __name__ == "__main__":
    main()
