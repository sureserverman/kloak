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
  for (int code = first; code <= last; code++) {
    if (ioctl(fd, req, code) < 0) {
      return -1;
    }
  }
  return 0;
}

int uinput_open(void) {
  int fd = open(UINPUT_DEV_PATH, O_WRONLY | O_NONBLOCK | O_CLOEXEC);
  if (fd < 0) {
    return -1;
  }

  if (ioctl(fd, UI_SET_EVBIT, EV_SYN) < 0) goto fail;
  if (ioctl(fd, UI_SET_EVBIT, EV_KEY) < 0) goto fail;
  if (ioctl(fd, UI_SET_EVBIT, EV_REL) < 0) goto fail;
  if (ioctl(fd, UI_SET_EVBIT, EV_ABS) < 0) goto fail;
  if (ioctl(fd, UI_SET_EVBIT, EV_MSC) < 0) goto fail;

  /*
   * Advertise every KEY_ / BTN_ code the kernel knows about. libinput will
   * only hand us codes that exist on real devices, so over-advertising here
   * is harmless and saves us from enumerating every keyboard/mouse button
   * by hand.
   */
  if (set_bits_range(fd, UI_SET_KEYBIT, 1, KEY_MAX) < 0) goto fail;

  if (ioctl(fd, UI_SET_RELBIT, REL_X) < 0) goto fail;
  if (ioctl(fd, UI_SET_RELBIT, REL_Y) < 0) goto fail;
  if (ioctl(fd, UI_SET_RELBIT, REL_WHEEL) < 0) goto fail;
  if (ioctl(fd, UI_SET_RELBIT, REL_HWHEEL) < 0) goto fail;
#ifdef REL_WHEEL_HI_RES
  if (ioctl(fd, UI_SET_RELBIT, REL_WHEEL_HI_RES) < 0) goto fail;
  if (ioctl(fd, UI_SET_RELBIT, REL_HWHEEL_HI_RES) < 0) goto fail;
#endif

  if (ioctl(fd, UI_SET_MSCBIT, MSC_SCAN) < 0) goto fail;

  struct uinput_abs_setup abs_x = {
    .code = ABS_X,
    .absinfo = { .minimum = 0, .maximum = KLOAK_ABS_MAX, .resolution = 1 },
  };
  struct uinput_abs_setup abs_y = {
    .code = ABS_Y,
    .absinfo = { .minimum = 0, .maximum = KLOAK_ABS_MAX, .resolution = 1 },
  };
  if (ioctl(fd, UI_ABS_SETUP, &abs_x) < 0) goto fail;
  if (ioctl(fd, UI_ABS_SETUP, &abs_y) < 0) goto fail;

  struct uinput_setup setup = {
    .id = {
      .bustype = BUS_VIRTUAL,
      .vendor  = 0x6B6C, /* "kl" */
      .product = 0x6F61, /* "oa" */
      .version = 1,
    },
  };
  strncpy(setup.name, "kloak", UINPUT_MAX_NAME_SIZE - 1);
  if (ioctl(fd, UI_DEV_SETUP, &setup) < 0) goto fail;
  if (ioctl(fd, UI_DEV_CREATE) < 0) goto fail;

  return fd;

fail: {
    int saved = errno;
    close(fd);
    errno = saved;
    return -1;
  }
}

int uinput_emit(int fd, uint16_t type, uint16_t code, int32_t value) {
  struct input_event ev;
  memset(&ev, 0, sizeof(ev));
  ev.type = type;
  ev.code = code;
  ev.value = value;
  ssize_t n = write(fd, &ev, sizeof(ev));
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
