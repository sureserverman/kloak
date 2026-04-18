/*
 * uinput output backend for kloak.
 *
 * kloak re-emits libinput events (after the jitter delay) by writing evdev
 * records to a kernel uinput device. The kernel then fans those events out
 * to every userspace consumer — X server, Wayland compositor, tty — so this
 * backend is compositor-agnostic.
 *
 * Requires CAP_SYS_ADMIN (kloak.service already grants it).
 */
#ifndef KLOAK_UINPUT_H
#define KLOAK_UINPUT_H

#include <stdint.h>

/*
 * Open /dev/uinput, declare all event codes kloak may emit (every EV_KEY,
 * EV_REL X/Y/WHEEL/HWHEEL, EV_ABS X/Y, EV_SYN, EV_MSC), and create the
 * virtual device. Returns the fd on success, -1 on failure (errno set).
 * The resulting device is named "kloak" with BUS_VIRTUAL.
 */
int uinput_open(void);

/*
 * Emit one evdev record. Thin wrapper around write(). Returns 0 on success,
 * -1 on failure (errno set).
 */
int uinput_emit(int fd, uint16_t type, uint16_t code, int32_t value);

/* Emit EV_SYN/SYN_REPORT/0 — call once per logical event group. */
int uinput_syn(int fd);

/* UI_DEV_DESTROY + close(). Idempotent on fd < 0. */
void uinput_close(int fd);

#endif /* KLOAK_UINPUT_H */
