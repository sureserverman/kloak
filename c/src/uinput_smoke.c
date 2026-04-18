/*
 * Smoke test for uinput.c — opens /dev/uinput, waits 2s for desktops to
 * notice the new device, types "hi\n", then tears down.
 *
 * Run as root:  sudo ./c/uinput-smoke
 *
 * Focus a text field before running. Success = "hi" plus a newline arrive
 * in the focused app.
 */
#include "uinput.h"

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <linux/input-event-codes.h>

static int press(int fd, uint16_t code) {
  if (uinput_emit(fd, EV_KEY, code, 1) < 0) return -1;
  if (uinput_syn(fd) < 0) return -1;
  if (uinput_emit(fd, EV_KEY, code, 0) < 0) return -1;
  if (uinput_syn(fd) < 0) return -1;
  usleep(40000);
  return 0;
}

int main(void) {
  int fd = uinput_open();
  if (fd < 0) {
    perror("uinput_open");
    fprintf(stderr, "(need root and /dev/uinput present)\n");
    return 1;
  }

  /* Let the desktop's input plumbing notice the new device. */
  sleep(2);

  if (press(fd, KEY_H) < 0) { perror("press H"); goto out; }
  if (press(fd, KEY_I) < 0) { perror("press I"); goto out; }
  if (press(fd, KEY_ENTER) < 0) { perror("press ENTER"); goto out; }

  sleep(1);

out:
  uinput_close(fd);
  return 0;
}
