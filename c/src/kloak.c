/*
 * Copyright (c) 2025 - 2025 ENCRYPTED SUPPORT LLC <adrelanos@whonix.org>
 * See the file COPYING for copying conditions.
 */

/*
 * kloak — keystroke / mouse timing anonymizer.
 *
 * Reads all /dev/input/event* devices via libinput (grabbing each exclusively
 * so userspace cannot see raw events), buffers events with randomized delays,
 * and re-emits them through a kernel uinput device. Because the output is at
 * kernel level, any graphical stack — GNOME Mutter, KDE KWin, wlroots
 * compositors, Xorg, bare tty — treats kloak's events like a regular hardware
 * keyboard/mouse.
 *
 * NOTES FOR DEVELOPERS:
 * - Use signed arithmetic wherever possible. Any form of integer
 *   over/underflow is dangerous here, thus kloak has -ftrapv enabled and thus
 *   signed arithmetic over/underflow will simply crash (and thus restart)
 *   kloak rather than resulting in memory corruption. Unsigned over/underflow
 *   however does NOT trap because it is well-defined in C. Thus avoid
 *   unsigned arithmetic wherever possible.
 * - Use an assert to check that a value is within bounds before every cast.
 */

#define _GNU_SOURCE

#include <assert.h>
#include <ctype.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <limits.h>
#include <math.h>
#include <poll.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/queue.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#include <linux/input.h>

#include <libinput.h>

#include "uinput.h"

/*********************************/
/* static defines                */
/*********************************/

/*
 * See the scroll handling comment in queue_libinput_event() for details on
 * why the scroll-related values below were chosen.
 */
#define SCROLL_UNITS_PER_TICK 120
#define SCROLL_UNITS_PER_TICK_D 120.0
#define SCROLL_ANGLE_TO_UNITS_FACTOR_D 8.0

#define POLL_FD_COUNT 2
#define INOTIFY_READ_BUF_LEN 16384
#define DEFAULT_MAX_DELAY_MS 100
#define DEFAULT_STARTUP_TIMEOUT_MS 500

#ifndef min
#define min(a, b) ( ((a) < (b)) ? (a) : (b) )
#endif
#ifndef max
#define max(a, b) ( ((a) > (b)) ? (a) : (b) )
#endif

/*******************/
/* core structures */
/*******************/

struct li_device_info {
  struct libinput_device *device;
  char *device_name;
  uint32_t refcount;
  LIST_ENTRY(li_device_info) entries;
};

struct key_name_value {
  const char *name;
  const uint32_t value;
};

enum input_packet_type {
  KLOAK_PACKET_TYPE_KEY,    /* EV_KEY keyboard key */
  KLOAK_PACKET_TYPE_BUTTON, /* EV_KEY pointer button */
  KLOAK_PACKET_TYPE_MOTION, /* EV_REL X/Y */
  KLOAK_PACKET_TYPE_SCROLL, /* EV_REL WHEEL/HWHEEL in ticks */
};

union input_packet_data {
  struct { uint32_t code; int32_t value; } key;     /* value: 1=press, 0=release */
  struct { int32_t dx; int32_t dy; } motion;
  struct { int32_t vert_ticks; int32_t horiz_ticks; } scroll;
};

struct input_packet {
  enum input_packet_type packet_type;
  int64_t sched_time;
  union input_packet_data data;
  TAILQ_ENTRY(input_packet) entries;
};

union rand_int64 {
  int64_t val;
  char raw[sizeof(int64_t)];
};

/********************/
/* global variables */
/********************/

static struct libinput *li = NULL;
static int uinput_fd = -1;
static int inotify_fd = -1;
static int randfd = -1;
static int64_t start_time = 0;

static struct pollfd ev_fds[POLL_FD_COUNT];

static int64_t prev_release_time = 0;
static TAILQ_HEAD(tailhead_evq, input_packet) evq_head;
static LIST_HEAD(listhead_ldi, li_device_info) ldi_head;

static int32_t max_delay = DEFAULT_MAX_DELAY_MS;
static int32_t startup_delay = DEFAULT_STARTUP_TIMEOUT_MS;
static bool enable_natural_scrolling = false;

static double vert_scroll_accum = 0.0;
static double horiz_scroll_accum = 0.0;

static uint32_t **esc_key_list = NULL;
static size_t *esc_key_sublist_len = NULL;
static bool *active_esc_key_list = NULL;
static size_t esc_key_list_len = 0;
static const char *default_esc_key_str = "KEY_RIGHTSHIFT,KEY_ESC";

static struct key_name_value key_table[] = {
  {"KEY_ESC", KEY_ESC},
  {"KEY_1", KEY_1}, {"KEY_2", KEY_2}, {"KEY_3", KEY_3}, {"KEY_4", KEY_4},
  {"KEY_5", KEY_5}, {"KEY_6", KEY_6}, {"KEY_7", KEY_7}, {"KEY_8", KEY_8},
  {"KEY_9", KEY_9}, {"KEY_0", KEY_0},
  {"KEY_MINUS", KEY_MINUS}, {"KEY_EQUAL", KEY_EQUAL},
  {"KEY_BACKSPACE", KEY_BACKSPACE}, {"KEY_TAB", KEY_TAB},
  {"KEY_Q", KEY_Q}, {"KEY_W", KEY_W}, {"KEY_E", KEY_E}, {"KEY_R", KEY_R},
  {"KEY_T", KEY_T}, {"KEY_Y", KEY_Y}, {"KEY_U", KEY_U}, {"KEY_I", KEY_I},
  {"KEY_O", KEY_O}, {"KEY_P", KEY_P},
  {"KEY_LEFTBRACE", KEY_LEFTBRACE}, {"KEY_RIGHTBRACE", KEY_RIGHTBRACE},
  {"KEY_ENTER", KEY_ENTER}, {"KEY_LEFTCTRL", KEY_LEFTCTRL},
  {"KEY_A", KEY_A}, {"KEY_S", KEY_S}, {"KEY_D", KEY_D}, {"KEY_F", KEY_F},
  {"KEY_G", KEY_G}, {"KEY_H", KEY_H}, {"KEY_J", KEY_J}, {"KEY_K", KEY_K},
  {"KEY_L", KEY_L},
  {"KEY_SEMICOLON", KEY_SEMICOLON}, {"KEY_APOSTROPHE", KEY_APOSTROPHE},
  {"KEY_GRAVE", KEY_GRAVE}, {"KEY_LEFTSHIFT", KEY_LEFTSHIFT},
  {"KEY_BACKSLASH", KEY_BACKSLASH},
  {"KEY_Z", KEY_Z}, {"KEY_X", KEY_X}, {"KEY_C", KEY_C}, {"KEY_V", KEY_V},
  {"KEY_B", KEY_B}, {"KEY_N", KEY_N}, {"KEY_M", KEY_M},
  {"KEY_COMMA", KEY_COMMA}, {"KEY_DOT", KEY_DOT}, {"KEY_SLASH", KEY_SLASH},
  {"KEY_RIGHTSHIFT", KEY_RIGHTSHIFT}, {"KEY_KPASTERISK", KEY_KPASTERISK},
  {"KEY_LEFTALT", KEY_LEFTALT}, {"KEY_SPACE", KEY_SPACE},
  {"KEY_CAPSLOCK", KEY_CAPSLOCK},
  {"KEY_F1", KEY_F1}, {"KEY_F2", KEY_F2}, {"KEY_F3", KEY_F3},
  {"KEY_F4", KEY_F4}, {"KEY_F5", KEY_F5}, {"KEY_F6", KEY_F6},
  {"KEY_F7", KEY_F7}, {"KEY_F8", KEY_F8}, {"KEY_F9", KEY_F9},
  {"KEY_F10", KEY_F10}, {"KEY_NUMLOCK", KEY_NUMLOCK},
  {"KEY_SCROLLLOCK", KEY_SCROLLLOCK},
  {"KEY_KP7", KEY_KP7}, {"KEY_KP8", KEY_KP8}, {"KEY_KP9", KEY_KP9},
  {"KEY_KPMINUS", KEY_KPMINUS},
  {"KEY_KP4", KEY_KP4}, {"KEY_KP5", KEY_KP5}, {"KEY_KP6", KEY_KP6},
  {"KEY_KPPLUS", KEY_KPPLUS},
  {"KEY_KP1", KEY_KP1}, {"KEY_KP2", KEY_KP2}, {"KEY_KP3", KEY_KP3},
  {"KEY_KP0", KEY_KP0}, {"KEY_KPDOT", KEY_KPDOT},
  {"KEY_ZENKAKUHANKAKU", KEY_ZENKAKUHANKAKU}, {"KEY_102ND", KEY_102ND},
  {"KEY_F11", KEY_F11}, {"KEY_F12", KEY_F12},
  {"KEY_RO", KEY_RO}, {"KEY_KATAKANA", KEY_KATAKANA},
  {"KEY_HIRAGANA", KEY_HIRAGANA}, {"KEY_HENKAN", KEY_HENKAN},
  {"KEY_KATAKANAHIRAGANA", KEY_KATAKANAHIRAGANA}, {"KEY_MUHENKAN", KEY_MUHENKAN},
  {"KEY_KPJPCOMMA", KEY_KPJPCOMMA}, {"KEY_KPENTER", KEY_KPENTER},
  {"KEY_RIGHTCTRL", KEY_RIGHTCTRL}, {"KEY_KPSLASH", KEY_KPSLASH},
  {"KEY_SYSRQ", KEY_SYSRQ}, {"KEY_RIGHTALT", KEY_RIGHTALT},
  {"KEY_LINEFEED", KEY_LINEFEED},
  {"KEY_HOME", KEY_HOME}, {"KEY_UP", KEY_UP}, {"KEY_PAGEUP", KEY_PAGEUP},
  {"KEY_LEFT", KEY_LEFT}, {"KEY_RIGHT", KEY_RIGHT}, {"KEY_END", KEY_END},
  {"KEY_DOWN", KEY_DOWN}, {"KEY_PAGEDOWN", KEY_PAGEDOWN},
  {"KEY_INSERT", KEY_INSERT}, {"KEY_DELETE", KEY_DELETE},
  {"KEY_MACRO", KEY_MACRO}, {"KEY_MUTE", KEY_MUTE},
  {"KEY_VOLUMEDOWN", KEY_VOLUMEDOWN}, {"KEY_VOLUMEUP", KEY_VOLUMEUP},
  {"KEY_POWER", KEY_POWER}, {"KEY_POWER2", KEY_POWER2},
  {"KEY_KPEQUAL", KEY_KPEQUAL}, {"KEY_KPPLUSMINUS", KEY_KPPLUSMINUS},
  {"KEY_PAUSE", KEY_PAUSE}, {"KEY_SCALE", KEY_SCALE},
  {"KEY_KPCOMMA", KEY_KPCOMMA}, {"KEY_HANGEUL", KEY_HANGEUL},
  {"KEY_HANGUEL", KEY_HANGUEL}, {"KEY_HANJA", KEY_HANJA}, {"KEY_YEN", KEY_YEN},
  {"KEY_LEFTMETA", KEY_LEFTMETA}, {"KEY_RIGHTMETA", KEY_RIGHTMETA},
  {"KEY_COMPOSE", KEY_COMPOSE},
  {"KEY_F13", KEY_F13}, {"KEY_F14", KEY_F14}, {"KEY_F15", KEY_F15},
  {"KEY_F16", KEY_F16}, {"KEY_F17", KEY_F17}, {"KEY_F18", KEY_F18},
  {"KEY_F19", KEY_F19}, {"KEY_F20", KEY_F20}, {"KEY_F21", KEY_F21},
  {"KEY_F22", KEY_F22}, {"KEY_F23", KEY_F23}, {"KEY_F24", KEY_F24},
  {"KEY_UNKNOWN", KEY_UNKNOWN},
  {NULL, 0}
};

/*********************/
/* utility functions */
/*********************/

static void *safe_calloc(size_t nmemb, size_t size) {
  void *p = calloc(nmemb, size);
  if (p == NULL) {
    fprintf(stderr, "FATAL ERROR: Could not allocate memory: %s\n",
      strerror(errno));
    exit(1);
  }
  return p;
}

static void *safe_reallocarray(void *ptr, size_t nmemb, size_t size) {
  void *p = reallocarray(ptr, nmemb, size);
  if (p == NULL) {
    fprintf(stderr, "FATAL ERROR: Could not allocate memory: %s\n",
      strerror(errno));
    exit(1);
  }
  return p;
}

static char *safe_strdup(const char *s) {
  char *p = strdup(s);
  if (p == NULL) {
    fprintf(stderr, "FATAL ERROR: Could not allocate memory: %s\n",
      strerror(errno));
    exit(1);
  }
  return p;
}

static int safe_open(const char *pathname, int flags) {
  int fd = open(pathname, flags);
  if (fd == -1) {
    fprintf(stderr, "FATAL ERROR: Could not open file '%s': %s\n",
      pathname, strerror(errno));
    exit(1);
  }
  return fd;
}

static void safe_close(int fd) {
  if (close(fd) == -1) {
    fprintf(stderr, "FATAL ERROR: Could not close a file: %s\n",
      strerror(errno));
    exit(1);
  }
}

static DIR *safe_opendir(const char *name) {
  DIR *dp = opendir(name);
  int dfd = 0;
  if (dp == NULL) {
    fprintf(stderr, "FATAL ERROR: Could not open directory '%s': %s\n",
      name, strerror(errno));
    exit(1);
  }
  dfd = dirfd(dp);
  if (dfd == -1 || fcntl(dfd, F_SETFD, FD_CLOEXEC) == -1) {
    fprintf(stderr, "FATAL ERROR: Could not set FD_CLOEXEC on '%s': %s\n",
      name, strerror(errno));
    exit(1);
  }
  return dp;
}

static void safe_closedir(DIR *dp) {
  if (closedir(dp) == -1) {
    fprintf(stderr, "FATAL ERROR: Could not close a directory: %s\n",
      strerror(errno));
    exit(1);
  }
}

static void li_device_info_unref(struct li_device_info *ldi) {
  ldi->refcount -= 1;
  if (ldi->refcount == 0) {
    libinput_device_set_user_data(ldi->device, NULL);
    free(ldi->device_name);
    free(ldi);
  }
}

static void read_random(char *buf, ssize_t len) {
  assert(len >= 0);
  assert(randfd > 0);
  assert(buf != NULL);

  if (read(randfd, buf, (size_t)(len)) < len) {
    fprintf(stderr, "FATAL ERROR: Could not read %ld byte(s) from /dev/urandom!\n", len);
    exit(1);
  }
}

static int64_t current_time_ms(void) {
  struct timespec spec = { 0 };
  int64_t result = 0;

  clock_gettime(CLOCK_MONOTONIC, &spec);
  assert(spec.tv_sec < INT64_MAX);
  result = ((int64_t)spec.tv_sec * 1000) + (spec.tv_nsec / 1000000);
  assert(result >= 0);
  if (start_time == 0) {
    start_time = result;
    return 0;
  }
  return result - start_time;
}

static int64_t random_between(int64_t lower, int64_t upper) {
  int64_t maxval = 0;
  union rand_int64 randval;

  assert(lower >= 0);
  assert(upper >= 0);

  if (lower >= upper) {
    return upper;
  }

  maxval = upper - lower + 1;
  assert(maxval > 0);
  do {
    read_random(randval.raw, sizeof(int64_t));
    if (randval.val == INT64_MIN) {
      randval.val = 0;
    } else {
      randval.val = llabs(randval.val);
    }
  } while (randval.val >= (INT64_MAX - (INT64_MAX % maxval)));

  randval.val %= maxval;
  return lower + randval.val;
}

static int32_t parse_uint31_arg(const char *arg_name, const char *val, int base) {
  char *end = NULL;
  uint64_t v = 0;
  errno = 0;
  v = strtoul(val, &end, base);
  if (errno == ERANGE || *end != '\0' || v > INT32_MAX) {
    fprintf(stderr, "FATAL ERROR: Invalid value '%s' passed to '%s'!\n",
      val, arg_name);
    exit(1);
  }
  return (int32_t)(v);
}

static int32_t sleep_ms(int64_t ms) {
  struct timespec ts = { 0 };
  int r = 0;

  assert(ms >= 0);
  ts.tv_sec = (time_t)(ms / 1000);
  ts.tv_nsec = (ms % 1000) * 1000000;
  do {
    r = nanosleep(&ts, &ts);
  } while (r == -1 && errno == EINTR);
  return r == -1 ? -1 : 0;
}

static char *sgenprintf(const char *fmt, ...) {
  char *rslt = NULL;
  int len = 0;
  va_list ap;

  va_start(ap, fmt);
  len = vsnprintf(NULL, 0, fmt, ap) + 1;
  va_end(ap);
  assert(len > 0);
  rslt = safe_calloc(1, (size_t)(len));
  va_start(ap, fmt);
  vsnprintf(rslt, (size_t)(len), fmt, ap);
  va_end(ap);
  return rslt;
}

static uint32_t lookup_keycode(const char *name) {
  struct key_name_value *p;
  for (p = key_table; p->name != NULL; p++) {
    if (strcmp(p->name, name) == 0) {
      return p->value;
    }
  }
  return 0;
}

/********************************/
/* libinput device bookkeeping  */
/********************************/

static int li_open_restricted(const char *path, int flags,
    __attribute__((unused)) void *user_data) {
  int fd = safe_open(path, flags | O_CLOEXEC);
  int one = 1;

  /*
   * Exclusive-grab the evdev device so the raw hardware events go only to
   * kloak. Other userspace sees our re-injected events from uinput instead.
   * Without this grab, keystrokes would be processed twice.
   */
  if (ioctl(fd, EVIOCGRAB, &one) < 0) {
    fprintf(stderr, "FATAL ERROR: Could not grab evdev device '%s'!\n", path);
    exit(1);
  }
  return fd < 0 ? -errno : fd;
}

static void li_close_restricted(int fd,
    __attribute__((unused)) void *user_data) {
  safe_close(fd);
}

static const struct libinput_interface li_interface = {
  .open_restricted = li_open_restricted,
  .close_restricted = li_close_restricted,
};

static void attach_input_device(const char *dev_name);
static void detach_input_device(const char *dev_name);

static void attach_input_device(const char *dev_name) {
  bool found = false;
  struct libinput_device *new_dev = NULL;
  char *dev_path = NULL;
  struct li_device_info *ldi = NULL;

  LIST_FOREACH(ldi, &ldi_head, entries) {
    if (strcmp(ldi->device_name, dev_name) == 0) {
      found = true;
      break;
    }
  }
  if (found) {
    /* Hot-unplug race: remove then re-add. */
    detach_input_device(dev_name);
  }

  dev_path = sgenprintf("/dev/input/%s", dev_name);
  new_dev = libinput_path_add_device(li, dev_path);
  free(dev_path);
  if (new_dev == NULL) {
    return;
  }

  if (enable_natural_scrolling &&
      libinput_device_config_scroll_has_natural_scroll(new_dev) != 0) {
    libinput_device_config_scroll_set_natural_scroll_enabled(new_dev, 1);
  }

  ldi = safe_calloc(1, sizeof(struct li_device_info));
  ldi->device = new_dev;
  ldi->device_name = safe_strdup(dev_name);
  ldi->refcount = 1;
  libinput_device_set_user_data(new_dev, ldi);
  LIST_INSERT_HEAD(&ldi_head, ldi, entries);
}

static void detach_input_device(const char *dev_name) {
  struct li_device_info *ldi = NULL;
  struct libinput_device *dev = NULL;
  bool found = false;

  LIST_FOREACH(ldi, &ldi_head, entries) {
    if (strcmp(ldi->device_name, dev_name) == 0) {
      found = true;
      break;
    }
  }
  if (!found) return;

  dev = ldi->device;
  LIST_REMOVE(ldi, entries);
  /*
   * Unref the bookkeeping node BEFORE removing the libinput device; libinput
   * may free the device struct if its refcount drops to zero, and
   * li_device_info_unref() touches libinput_device_set_user_data().
   */
  li_device_info_unref(ldi);
  libinput_path_remove_device(dev);
}

/***************************/
/* scroll tick accumulator */
/***************************/

static int32_t get_ticks_from_scroll_accum(double *accum_ptr) {
  double scroll_accum = *accum_ptr;
  double scroll_ticks_d = 0.0;
  int32_t scroll_ticks = 0;

  if (fpclassify(scroll_accum) != FP_ZERO) {
    assert(isfinite(scroll_accum));
    scroll_ticks_d = scroll_accum / SCROLL_UNITS_PER_TICK_D;
    assert(scroll_ticks_d <= (INT32_MAX / SCROLL_UNITS_PER_TICK));
    assert(scroll_ticks_d >= (INT32_MIN / SCROLL_UNITS_PER_TICK));
    scroll_ticks = (int32_t)(scroll_ticks_d);
    if (scroll_ticks != 0) {
      scroll_accum += -(scroll_ticks * SCROLL_UNITS_PER_TICK);
      *accum_ptr = scroll_accum;
    }
  }
  return scroll_ticks;
}

/*********************/
/* escape-key combo  */
/*********************/

static void parse_esc_key_str(const char *esc_key_str) {
  char *copy = safe_strdup(esc_key_str);
  char *orig = copy;
  char *root_token = NULL;
  const char *sub_token = NULL;
  size_t i, j;

  for (i = 0; (root_token = strsep(&copy, ",")) != NULL; i++) {
    if (root_token[0] == '\0') {
      fprintf(stderr, "FATAL ERROR: Empty key name in escape key list!\n");
      exit(1);
    }
    esc_key_list_len++;
    esc_key_list = safe_reallocarray(esc_key_list, esc_key_list_len,
      sizeof(uint32_t *));
    esc_key_sublist_len = safe_reallocarray(esc_key_sublist_len,
      esc_key_list_len, sizeof(size_t));
    active_esc_key_list = safe_reallocarray(active_esc_key_list,
      esc_key_list_len, sizeof(bool));
    esc_key_list[i] = NULL;
    esc_key_sublist_len[i] = 0;
    active_esc_key_list[i] = false;

    for (j = 0; (sub_token = strsep(&root_token, "|")) != NULL; j++) {
      if (sub_token[0] == '\0') {
        fprintf(stderr, "FATAL ERROR: Empty key name in escape key list!\n");
        exit(1);
      }
      esc_key_sublist_len[i]++;
      esc_key_list[i] = safe_reallocarray(esc_key_list[i],
        esc_key_sublist_len[i], sizeof(uint32_t));
      esc_key_list[i][j] = lookup_keycode(sub_token);
      if (esc_key_list[i][j] == 0) {
        fprintf(stderr, "FATAL ERROR: Unrecognized Key name '%s'!\n", sub_token);
        exit(1);
      }
    }
  }
  free(orig);
}

static void register_esc_combo_event(struct libinput_event *li_event) {
  struct libinput_event_keyboard *kb_event = NULL;
  uint32_t key = 0;
  enum libinput_key_state key_state;
  size_t i, j;
  bool hit_exit = true;

  if (libinput_event_get_type(li_event) != LIBINPUT_EVENT_KEYBOARD_KEY) {
    return;
  }
  kb_event = libinput_event_get_keyboard_event(li_event);
  key = libinput_event_keyboard_get_key(kb_event);
  key_state = libinput_event_keyboard_get_key_state(kb_event);

  for (i = 0; i < esc_key_list_len; i++) {
    for (j = 0; j < esc_key_sublist_len[i]; j++) {
      if (esc_key_list[i][j] != key) continue;
      active_esc_key_list[i] = (key_state == LIBINPUT_KEY_STATE_PRESSED);
      break;
    }
  }
  for (i = 0; i < esc_key_list_len; i++) {
    if (!active_esc_key_list[i]) {
      hit_exit = false;
      break;
    }
  }
  if (hit_exit) {
    exit(0);
  }
}

/**********************/
/* event queue + emit */
/**********************/

static void enqueue_packet(struct input_packet *pkt) {
  int64_t now = current_time_ms();
  int64_t lower = min(max(prev_release_time - now, 0), max_delay);
  int64_t delay = random_between(lower, max_delay);
  pkt->sched_time = now + delay;
  TAILQ_INSERT_TAIL(&evq_head, pkt, entries);
  prev_release_time = pkt->sched_time;
}

static void queue_key(uint32_t code, int32_t value) {
  struct input_packet *pkt = safe_calloc(1, sizeof(*pkt));
  pkt->packet_type = KLOAK_PACKET_TYPE_KEY;
  pkt->data.key.code = code;
  pkt->data.key.value = value;
  enqueue_packet(pkt);
}

static void queue_button(uint32_t code, int32_t value) {
  struct input_packet *pkt = safe_calloc(1, sizeof(*pkt));
  pkt->packet_type = KLOAK_PACKET_TYPE_BUTTON;
  pkt->data.key.code = code;
  pkt->data.key.value = value;
  enqueue_packet(pkt);
}

static void queue_motion(int32_t dx, int32_t dy) {
  struct input_packet *last = NULL;
  struct input_packet *pkt = NULL;

  /*
   * Coalesce with the trailing packet if it is also motion and not yet
   * released. Mouse motion comes in at hundreds of events per second; without
   * this, the queue grows unboundedly.
   */
  last = TAILQ_LAST(&evq_head, tailhead_evq);
  if (last != NULL && last->packet_type == KLOAK_PACKET_TYPE_MOTION &&
      last->sched_time > current_time_ms()) {
    int64_t sx = (int64_t)last->data.motion.dx + dx;
    int64_t sy = (int64_t)last->data.motion.dy + dy;
    if (sx >= INT32_MIN && sx <= INT32_MAX &&
        sy >= INT32_MIN && sy <= INT32_MAX) {
      last->data.motion.dx = (int32_t)sx;
      last->data.motion.dy = (int32_t)sy;
      return;
    }
  }
  pkt = safe_calloc(1, sizeof(*pkt));
  pkt->packet_type = KLOAK_PACKET_TYPE_MOTION;
  pkt->data.motion.dx = dx;
  pkt->data.motion.dy = dy;
  enqueue_packet(pkt);
}

static void queue_scroll(int32_t vert_ticks, int32_t horiz_ticks) {
  struct input_packet *pkt = NULL;

  if (vert_ticks == 0 && horiz_ticks == 0) return;
  pkt = safe_calloc(1, sizeof(*pkt));
  pkt->packet_type = KLOAK_PACKET_TYPE_SCROLL;
  pkt->data.scroll.vert_ticks = vert_ticks;
  pkt->data.scroll.horiz_ticks = horiz_ticks;
  enqueue_packet(pkt);
}

static void emit_packet(const struct input_packet *pkt) {
  switch (pkt->packet_type) {
    case KLOAK_PACKET_TYPE_KEY:
    case KLOAK_PACKET_TYPE_BUTTON:
      uinput_emit(uinput_fd, EV_KEY, (uint16_t)pkt->data.key.code,
        pkt->data.key.value);
      uinput_syn(uinput_fd);
      break;
    case KLOAK_PACKET_TYPE_MOTION:
      if (pkt->data.motion.dx != 0) {
        uinput_emit(uinput_fd, EV_REL, REL_X, pkt->data.motion.dx);
      }
      if (pkt->data.motion.dy != 0) {
        uinput_emit(uinput_fd, EV_REL, REL_Y, pkt->data.motion.dy);
      }
      uinput_syn(uinput_fd);
      break;
    case KLOAK_PACKET_TYPE_SCROLL:
      /*
       * REL_WHEEL is inverted relative to kloak's accumulator: a positive
       * accumulator value means the wheel rotated forward (away from user),
       * which by X/Wayland convention scrolls the page UP — that's a
       * positive REL_WHEEL. kloak's queue_scroll stores it in that sense
       * already, so no sign flip here.
       */
      if (pkt->data.scroll.vert_ticks != 0) {
        uinput_emit(uinput_fd, EV_REL, REL_WHEEL,
          pkt->data.scroll.vert_ticks);
      }
      if (pkt->data.scroll.horiz_ticks != 0) {
        uinput_emit(uinput_fd, EV_REL, REL_HWHEEL,
          pkt->data.scroll.horiz_ticks);
      }
      uinput_syn(uinput_fd);
      break;
    default:
      fprintf(stderr, "FATAL ERROR: Unknown input packet type %d\n",
        (int)pkt->packet_type);
      exit(1);
  }
}

static void release_scheduled_input_events(void) {
  int64_t now = current_time_ms();
  struct input_packet *pkt = NULL;

  while ((pkt = TAILQ_FIRST(&evq_head)) && now >= pkt->sched_time) {
    emit_packet(pkt);
    TAILQ_REMOVE(&evq_head, pkt, entries);
    free(pkt);
  }
}

/******************************/
/* libinput event translation */
/******************************/

static void handle_libinput_event(struct libinput_event *li_event) {
  enum libinput_event_type t = libinput_event_get_type(li_event);

  if (t == LIBINPUT_EVENT_DEVICE_ADDED) {
    struct libinput_device *d = libinput_event_get_device(li_event);
    int can_tap = libinput_device_config_tap_get_finger_count(d);
    if (can_tap) {
      libinput_device_config_tap_set_enabled(d, LIBINPUT_CONFIG_TAP_ENABLED);
    }
  } else if (t == LIBINPUT_EVENT_KEYBOARD_KEY) {
    struct libinput_event_keyboard *ke = libinput_event_get_keyboard_event(li_event);
    uint32_t key = libinput_event_keyboard_get_key(ke);
    enum libinput_key_state ks = libinput_event_keyboard_get_key_state(ke);
    queue_key(key, (ks == LIBINPUT_KEY_STATE_PRESSED) ? 1 : 0);
  } else if (t == LIBINPUT_EVENT_POINTER_BUTTON) {
    struct libinput_event_pointer *pe = libinput_event_get_pointer_event(li_event);
    uint32_t btn = libinput_event_pointer_get_button(pe);
    enum libinput_button_state bs = libinput_event_pointer_get_button_state(pe);
    queue_button(btn, (bs == LIBINPUT_BUTTON_STATE_PRESSED) ? 1 : 0);
  } else if (t == LIBINPUT_EVENT_POINTER_MOTION) {
    struct libinput_event_pointer *pe = libinput_event_get_pointer_event(li_event);
    double dx = libinput_event_pointer_get_dx(pe);
    double dy = libinput_event_pointer_get_dy(pe);
    /*
     * Round toward nearest, clamp to int32 range. Sub-pixel leftovers are
     * dropped — at 200Hz mouse polling and int32 output, the rounding error
     * is indistinguishable from hardware jitter.
     */
    int32_t idx = (int32_t)(dx < 0 ? dx - 0.5 : dx + 0.5);
    int32_t idy = (int32_t)(dy < 0 ? dy - 0.5 : dy + 0.5);
    if (idx != 0 || idy != 0) {
      queue_motion(idx, idy);
    }
  } else if (t == LIBINPUT_EVENT_POINTER_SCROLL_WHEEL) {
    /*
     * See the long comment on scroll handling in the original kloak.c for
     * the theory behind the 120-unit accumulator. Keeping the units and
     * factors identical so behavior matches the Wayland-era kloak.
     */
    struct libinput_event_pointer *pe = libinput_event_get_pointer_event(li_event);
    double vv = 0.0, hv = 0.0;
    if (libinput_event_pointer_has_axis(pe, LIBINPUT_POINTER_AXIS_SCROLL_VERTICAL)) {
      vv = libinput_event_pointer_get_scroll_value_v120(pe,
        LIBINPUT_POINTER_AXIS_SCROLL_VERTICAL);
    }
    if (libinput_event_pointer_has_axis(pe, LIBINPUT_POINTER_AXIS_SCROLL_HORIZONTAL)) {
      hv = libinput_event_pointer_get_scroll_value_v120(pe,
        LIBINPUT_POINTER_AXIS_SCROLL_HORIZONTAL);
    }
    vert_scroll_accum += vv;
    horiz_scroll_accum += hv;
    queue_scroll(get_ticks_from_scroll_accum(&vert_scroll_accum),
                 get_ticks_from_scroll_accum(&horiz_scroll_accum));
  } else if (t == LIBINPUT_EVENT_POINTER_SCROLL_FINGER ||
             t == LIBINPUT_EVENT_POINTER_SCROLL_CONTINUOUS) {
    struct libinput_event_pointer *pe = libinput_event_get_pointer_event(li_event);
    double vv = 0.0, hv = 0.0;
    if (libinput_event_pointer_has_axis(pe, LIBINPUT_POINTER_AXIS_SCROLL_VERTICAL)) {
      vv = libinput_event_pointer_get_scroll_value(pe,
        LIBINPUT_POINTER_AXIS_SCROLL_VERTICAL) * SCROLL_ANGLE_TO_UNITS_FACTOR_D;
    }
    if (libinput_event_pointer_has_axis(pe, LIBINPUT_POINTER_AXIS_SCROLL_HORIZONTAL)) {
      hv = libinput_event_pointer_get_scroll_value(pe,
        LIBINPUT_POINTER_AXIS_SCROLL_HORIZONTAL) * SCROLL_ANGLE_TO_UNITS_FACTOR_D;
    }
    vert_scroll_accum += vv;
    horiz_scroll_accum += hv;
    queue_scroll(get_ticks_from_scroll_accum(&vert_scroll_accum),
                 get_ticks_from_scroll_accum(&horiz_scroll_accum));
  }
  /* Gestures and other events intentionally dropped. */

  libinput_event_destroy(li_event);
}

/*********************/
/* inotify handling  */
/*********************/

static void handle_inotify_events(void) {
  static char *read_buf = NULL;
  ssize_t read_len = 0;
  ssize_t rem_len = 0;
  ssize_t struct_len = 0;
  struct inotify_event *ie;

  if (read_buf == NULL) {
    read_buf = safe_calloc(INOTIFY_READ_BUF_LEN, sizeof(char));
  }
  for (;;) {
    read_len = read(inotify_fd, read_buf, INOTIFY_READ_BUF_LEN);
    if (read_len == -1) {
      if (errno == EINTR) continue;
      fprintf(stderr, "FATAL ERROR: inotify read: %s\n", strerror(errno));
      exit(1);
    }
    break;
  }

  ie = (void *)read_buf;
  rem_len = read_len;
  while (true) {
    assert(rem_len >= (ssize_t)(sizeof(struct inotify_event)));
    assert(ie->len < SSIZE_MAX - sizeof(struct inotify_event));
    struct_len = ((ssize_t)(sizeof(struct inotify_event)) + (ssize_t)(ie->len));
    assert(struct_len <= rem_len);
    rem_len -= struct_len;
    assert(rem_len >= 0);

    if (strncmp(ie->name, "event", strlen("event")) == 0) {
      if (ie->mask & IN_CREATE) attach_input_device(ie->name);
      else                      detach_input_device(ie->name);
    }
    if (rem_len <= 0) break;
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wcast-align"
    ie = (struct inotify_event *)((char *)(ie) + struct_len);
#pragma GCC diagnostic pop
  }
}

/**********/
/*  init  */
/**********/

static int calc_poll_timeout(void) {
  struct input_packet *pkt = TAILQ_FIRST(&evq_head);
  int64_t dur = 0;
  if (pkt == NULL) return -1;
  dur = pkt->sched_time - current_time_ms();
  if (dur < 0) return 0;
  if (dur > INT_MAX) return INT_MAX;
  return (int)dur;
}

static void print_usage(void) {
  fprintf(stderr, "Usage: kloak [options]\n");
  fprintf(stderr, "Anonymizes keyboard and mouse input timing by randomly delaying events.\n");
  fprintf(stderr, "Works on any Linux graphical stack: GNOME, KDE, wlroots, Xorg, tty.\n\n");
  fprintf(stderr, "Options:\n");
  fprintf(stderr, "  -h, --help                      Print help.\n");
  fprintf(stderr, "  -d, --delay=milliseconds        Max delay of released events. Default 100.\n");
  fprintf(stderr, "  -s, --start-delay=milliseconds  Time to wait before startup. Default 500.\n");
  fprintf(stderr, "  -n, --natural-scrolling=(true|false)\n");
  fprintf(stderr, "                                  Natural scrolling. Default false.\n");
  fprintf(stderr, "  -k, --esc-key-combo=KEY_1[,KEY_2|KEY_3...]\n");
  fprintf(stderr, "                                  Exit-combo. Default KEY_RIGHTSHIFT,KEY_ESC.\n");
}

static void parse_cli_args(int argc, char **argv) {
  static struct option optarr[] = {
    {"delay", required_argument, NULL, 'd'},
    {"start-delay", required_argument, NULL, 's'},
    {"help", no_argument, NULL, 'h'},
    {"esc-key-combo", required_argument, NULL, 'k'},
    {"natural-scrolling", required_argument, NULL, 'n'},
    {0, 0, 0, 0}
  };
  int r = 0;
  while (true) {
    r = getopt_long(argc, argv, "d:s:hk:n:", optarr, NULL);
    if (r == -1) break;
    switch (r) {
      case 'd': max_delay = parse_uint31_arg("delay", optarg, 10); break;
      case 's': startup_delay = parse_uint31_arg("start-delay", optarg, 10); break;
      case 'n':
        enable_natural_scrolling = (strcmp(optarg, "true") == 0);
        break;
      case 'k': parse_esc_key_str(optarg); break;
      case 'h': print_usage(); exit(0);
      case '?':
      default:  print_usage(); exit(1);
    }
  }
  if (esc_key_list == NULL) {
    parse_esc_key_str(default_esc_key_str);
  }
}

static void applayer_random_init(void) {
  randfd = safe_open("/dev/urandom", O_RDONLY | O_CLOEXEC);
}

static void applayer_uinput_init(void) {
  uinput_fd = uinput_open();
  if (uinput_fd < 0) {
    fprintf(stderr, "FATAL ERROR: Could not open /dev/uinput: %s\n",
      strerror(errno));
    fprintf(stderr, "Ensure the 'uinput' kernel module is loaded and this process has CAP_SYS_ADMIN.\n");
    exit(1);
  }
}

static void applayer_libinput_init(void) {
  DIR *input_dir = NULL;
  struct dirent *entry = NULL;

  LIST_INIT(&ldi_head);
  TAILQ_INIT(&evq_head);

  li = libinput_path_create_context(&li_interface, NULL);
  if (li == NULL) {
    fprintf(stderr, "FATAL ERROR: Could not create libinput context.\n");
    exit(1);
  }

  input_dir = safe_opendir("/dev/input");
  while ((entry = readdir(input_dir)) != NULL) {
    if (entry->d_type != DT_CHR) continue;
    if (strncmp(entry->d_name, "event", strlen("event")) != 0) continue;
    attach_input_device(entry->d_name);
  }
  safe_closedir(input_dir);
}

static void applayer_inotify_init(void) {
  inotify_fd = inotify_init1(IN_CLOEXEC);
  if (inotify_fd == -1) {
    fprintf(stderr, "FATAL ERROR: Could not initialize inotify: %s\n",
      strerror(errno));
    exit(1);
  }
  if (inotify_add_watch(inotify_fd, "/dev/input", IN_CREATE | IN_DELETE) == -1) {
    fprintf(stderr, "FATAL ERROR: inotify watch on /dev/input: %s\n",
      strerror(errno));
    exit(1);
  }
}

static void applayer_poll_init(void) {
  memset(ev_fds, 0, sizeof(ev_fds));
  ev_fds[0].fd = libinput_get_fd(li);
  ev_fds[0].events = POLLIN;
  ev_fds[1].fd = inotify_fd;
  ev_fds[1].events = POLLIN;
}

/**********/
/*  MAIN  */
/**********/

int main(int argc, char **argv) {
  /*
   * BIG FAT WARNING: Do not attempt to build kloak with NDEBUG defined. Many
   * of the assertions in this code are essential for security, and building
   * kloak with NDEBUG defined will turn all of them off. Systems running a
   * build of kloak with NDEBUG defined should be treated as compromised if
   * they process any form of untrusted data.
   */
#ifdef NDEBUG
  fprintf(stderr,
    "FATAL ERROR: Built with NDEBUG set. kloak does not support this.\n");
  exit(1);
#endif

  if (getuid() != 0) {
    fprintf(stderr, "FATAL ERROR: Must be run as root!\n");
    exit(1);
  }
  if (setenv("LC_ALL", "C", 1) == -1) {
    fprintf(stderr, "FATAL ERROR: Could not set LC_ALL=C!\n");
    exit(1);
  }

  parse_cli_args(argc, argv);

  if (sleep_ms(startup_delay) != 0) {
    fprintf(stderr, "FATAL ERROR: startup sleep failed!\n");
    exit(1);
  }

  applayer_random_init();
  applayer_uinput_init();
  applayer_libinput_init();
  applayer_inotify_init();
  applayer_poll_init();

  while (true) {
    for (;;) {
      enum libinput_event_type t = libinput_next_event_type(li);
      struct libinput_event *e = NULL;
      if (t == LIBINPUT_EVENT_NONE) break;
      e = libinput_get_event(li);
      register_esc_combo_event(e);
      handle_libinput_event(e);
      /* handle_libinput_event destroys the event. */
    }

    release_scheduled_input_events();

    poll(ev_fds, POLL_FD_COUNT, calc_poll_timeout());

    if (ev_fds[0].revents & POLLIN) {
      libinput_dispatch(li);
    }
    ev_fds[0].revents = 0;

    if (ev_fds[1].revents & POLLIN) {
      handle_inotify_events();
    }
    ev_fds[1].revents = 0;
  }

  uinput_close(uinput_fd);
  return 0;
}
