/*
 * uinput output backend — see uinput.h for contract.
 */
#include "uinput.h"

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>
#include <linux/input.h>
#include <linux/input-event-codes.h>
#include <linux/uinput.h>
#include <sys/ioctl.h>

#define UINPUT_DEV_PATH "/dev/uinput"
#define KLOAK_ABS_MAX   65535

static int set_bits_range(int fd, unsigned long req, int first, int last) {
  int code;
  for (code = first; code <= last; code++) {
    if (ioctl(fd, req, code) < 0) {
      return -1;
    }
  }
  return 0;
}

int uinput_open(void) {
  int fd = -1;
  int saved_errno = 0;
  struct uinput_abs_setup abs_x;
  struct uinput_abs_setup abs_y;
  struct uinput_setup setup;

  memset(&abs_x, 0, sizeof(abs_x));
  abs_x.code = ABS_X;
  abs_x.absinfo.minimum = 0;
  abs_x.absinfo.maximum = KLOAK_ABS_MAX;
  abs_x.absinfo.resolution = 1;

  memset(&abs_y, 0, sizeof(abs_y));
  abs_y.code = ABS_Y;
  abs_y.absinfo.minimum = 0;
  abs_y.absinfo.maximum = KLOAK_ABS_MAX;
  abs_y.absinfo.resolution = 1;

  memset(&setup, 0, sizeof(setup));
  setup.id.bustype = BUS_VIRTUAL;
  setup.id.vendor  = 0x6B6C; /* "kl" */
  setup.id.product = 0x6F61; /* "oa" */
  setup.id.version = 1;
  strncpy(setup.name, "kloak", UINPUT_MAX_NAME_SIZE - 1);

  fd = open(UINPUT_DEV_PATH, O_WRONLY | O_NONBLOCK | O_CLOEXEC);
  if (fd < 0) {
    return -1;
  }

  if (ioctl(fd, UI_SET_EVBIT, EV_SYN) < 0) { saved_errno = errno; goto fail; }
  if (ioctl(fd, UI_SET_EVBIT, EV_KEY) < 0) { saved_errno = errno; goto fail; }
  if (ioctl(fd, UI_SET_EVBIT, EV_REL) < 0) { saved_errno = errno; goto fail; }
  if (ioctl(fd, UI_SET_EVBIT, EV_ABS) < 0) { saved_errno = errno; goto fail; }
  if (ioctl(fd, UI_SET_EVBIT, EV_MSC) < 0) { saved_errno = errno; goto fail; }

  /*
   * Advertise every KEY_ / BTN_ code the kernel knows about. libinput will
   * only hand us codes that exist on real devices, so over-advertising here
   * is harmless and saves us from enumerating every keyboard/mouse button
   * by hand.
   */
  if (set_bits_range(fd, UI_SET_KEYBIT, 1, KEY_MAX) < 0) {
    saved_errno = errno; goto fail;
  }

  if (ioctl(fd, UI_SET_RELBIT, REL_X) < 0) { saved_errno = errno; goto fail; }
  if (ioctl(fd, UI_SET_RELBIT, REL_Y) < 0) { saved_errno = errno; goto fail; }
  if (ioctl(fd, UI_SET_RELBIT, REL_WHEEL) < 0) { saved_errno = errno; goto fail; }
  if (ioctl(fd, UI_SET_RELBIT, REL_HWHEEL) < 0) { saved_errno = errno; goto fail; }
#ifdef REL_WHEEL_HI_RES
  if (ioctl(fd, UI_SET_RELBIT, REL_WHEEL_HI_RES) < 0) {
    saved_errno = errno; goto fail;
  }
  if (ioctl(fd, UI_SET_RELBIT, REL_HWHEEL_HI_RES) < 0) {
    saved_errno = errno; goto fail;
  }
#endif

  if (ioctl(fd, UI_SET_MSCBIT, MSC_SCAN) < 0) { saved_errno = errno; goto fail; }

  if (ioctl(fd, UI_ABS_SETUP, &abs_x) < 0) { saved_errno = errno; goto fail; }
  if (ioctl(fd, UI_ABS_SETUP, &abs_y) < 0) { saved_errno = errno; goto fail; }

  if (ioctl(fd, UI_DEV_SETUP, &setup) < 0) { saved_errno = errno; goto fail; }
  if (ioctl(fd, UI_DEV_CREATE) < 0) { saved_errno = errno; goto fail; }

  return fd;

fail:
  close(fd);
  errno = saved_errno;
  return -1;
}

int uinput_emit(int fd, uint16_t type, uint16_t code, int32_t value) {
  struct input_event ev;
  ssize_t n;

  memset(&ev, 0, sizeof(ev));
  ev.type = type;
  ev.code = code;
  ev.value = value;
  n = write(fd, &ev, sizeof(ev));
  return (n == (ssize_t)sizeof(ev)) ? 0 : -1;
}

int uinput_syn(int fd) {
  return uinput_emit(fd, EV_SYN, SYN_REPORT, 0);
}

void uinput_close(int fd) {
  if (fd < 0) return;
  ioctl(fd, UI_DEV_DESTROY);
  close(fd);
}
