#!/usr/bin/make -f

CC         ?= gcc
INSTALL    ?= install
PKG_CONFIG ?= pkg-config
RONN       ?= ronn

CFLAGS     ?= -O2 -g

# NOTE: The apparmor profile is hardcoded to use /usr/bin/kloak. So if you
#       change the default install paths, you will have to patch those files
#       yourself.
prefix         ?= /usr
bindir         ?= $(prefix)/bin
datadir        ?= $(prefix)/share
mandir         ?= $(datadir)/man
apparmor_dir   ?= /etc/apparmor.d/

TARGETARCH=$(shell $(CC) -dumpmachine)
CC_VERSION=$(shell $(CC) --version)

# https://www.kicksecure.com/wiki/Dev/compiler_hardening
WARN_CFLAGS := -Wall -Wextra -Wformat -Wformat=2 -Wconversion \
	-Wimplicit-fallthrough -Werror=format-security -Werror=implicit \
	-Werror=int-conversion -Werror=incompatible-pointer-types \
	-Wformat-overflow -Wformat-signedness -Wformat-truncation \
	-Wnull-dereference -Winit-self -Wmissing-include-dirs \
	-Wshift-negative-value -Wshift-overflow -Wswitch-default \
	-Wuninitialized -Walloca -Warray-bounds -Wfloat-equal -Wshadow \
	-Wpointer-arith -Wundef -Wunused-macros -Wbad-function-cast -Wcast-qual \
	-Wcast-align -Wwrite-strings -Wdate-time -Wstrict-prototypes \
	-Wold-style-definition -Wredundant-decls -Winvalid-utf8 -Wvla \
	-Wdisabled-optimization -Wstack-protector -Wdeclaration-after-statement

ifeq (,$(findstring clang,$(CC_VERSION))) # if not clang
# clang as for 19.1.0 doesn't support these warnings
WARN_CFLAGS += -Wtrampolines -Wbidi-chars=any,ucn -Wformat-overflow=2 \
	-Wformat-truncation=2 -Wshift-overflow=2 -Wtrivial-auto-var-init \
	-Wstringop-overflow=3 -Wstrict-flex-arrays -Walloc-zero \
	-Warray-bounds=2 -Wattribute-alias=2 \
	-Wduplicated-branches -Wduplicated-cond	-Wcast-align=strict \
	-Wjump-misses-init -Wlogical-op
endif

#ifeq (,$(findstring clang,$(CC_VERSION))) # if clang
#WARN_CFLAGS +=  #
#endif

# IMPORTANT: Do NOT remove -ftrapv from the list of flags, it is used to allow
# signed integer arithmetic without explicit overflow checks. Also make sure
# that -ftrapv ALWAYS comes after -fno-strict-overflow, as
# -fno-strict-overflow implies -fwrapv, and -ftrapv must override -fwrapv.
FORTIFY_CFLAGS := -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=3 \
	-fstack-clash-protection -fstack-protector-all \
	-fno-delete-null-pointer-checks -fno-strict-overflow -fno-strict-aliasing \
	-fstrict-flex-arrays=3 -ftrapv -ftrivial-auto-var-init=pattern

## kloak only officially supports Linux with the GNU C library. Therefore we
## only check *-linux-gnu platforms in our arch checks. Contributions are
## welcome if support for more kernels and/or C libraries is desirable.
ifeq (yes,$(patsubst x86_64%-linux-gnu,yes,$(TARGETARCH)))
FORTIFY_CFLAGS += -fcf-protection=full # only supported on x86_64
FORTIFY_CFLAGS += -fzero-call-used-regs=all # only known to be supported on x86_64 and aarch64
endif
ifeq (yes,$(patsubst aarch64%-linux-gnu,yes,$(TARGETARCH)))
FORTIFY_CFLAGS += -mbranch-protection=standard # only supported on aarch64
FORTIFY_CFLAGS += -fzero-call-used-regs=all # see above
endif

BIN_CFLAGS := -fPIE

CFLAGS := $(WARN_CFLAGS) $(FORTIFY_CFLAGS) $(BIN_CFLAGS) $(CFLAGS)
LDFLAGS := -Wl,-z,nodlopen -Wl,-z,noexecstack -Wl,-z,relro -Wl,-z,now \
	-Wl,-z,separate-code -Wl,--as-needed -Wl,--no-copy-dt-needed-entries \
	-pie $(LDFLAGS)

ifeq (, $(shell which $(PKG_CONFIG)))
$(error pkg-config not installed!)
endif

all : kloak

kloak : src/kloak.c src/kloak.h src/xdg-shell-protocol.h src/xdg-shell-protocol.c src/xdg-output-protocol.h src/xdg-output-protocol.c src/wlr-layer-shell.c src/wlr-layer-shell.h src/wlr-virtual-pointer.c src/wlr-virtual-pointer.h src/virtual-keyboard.c src/virtual-keyboard.h
	## Comment out the below line and uncomment the line after it to do a test build with -fanalyzer
	$(CC) -g src/kloak.c src/xdg-shell-protocol.c src/xdg-output-protocol.c src/wlr-layer-shell.c src/wlr-virtual-pointer.c src/virtual-keyboard.c -o kloak -lm -lrt $(shell $(PKG_CONFIG) --cflags --libs libinput) $(shell $(PKG_CONFIG) --cflags --libs libevdev) $(shell $(PKG_CONFIG) --cflags --libs wayland-client) $(shell $(PKG_CONFIG) --cflags --libs xkbcommon) $(CFLAGS) $(LDFLAGS)
	#$(CC) -g src/kloak.c src/xdg-shell-protocol.c src/xdg-output-protocol.c src/wlr-layer-shell.c src/wlr-virtual-pointer.c src/virtual-keyboard.c -o kloak -lm -lrt $(shell $(PKG_CONFIG) --cflags --libs libinput) $(shell $(PKG_CONFIG) --cflags --libs libevdev) $(shell $(PKG_CONFIG) --cflags --libs wayland-client) $(shell $(PKG_CONFIG) --cflags --libs xkbcommon) -fanalyzer

src/xdg-shell-protocol.h : protocol/xdg-shell.xml
	wayland-scanner client-header < protocol/xdg-shell.xml > src/xdg-shell-protocol.h

src/xdg-shell-protocol.c : protocol/xdg-shell.xml
	wayland-scanner private-code < protocol/xdg-shell.xml > src/xdg-shell-protocol.c

src/xdg-output-protocol.h : protocol/xdg-output-unstable-v1.xml
	wayland-scanner client-header < protocol/xdg-output-unstable-v1.xml > src/xdg-output-protocol.h

src/xdg-output-protocol.c : protocol/xdg-output-unstable-v1.xml
	wayland-scanner private-code < protocol/xdg-output-unstable-v1.xml > src/xdg-output-protocol.c

src/wlr-layer-shell.h : protocol/wlr-layer-shell-unstable-v1.xml
	wayland-scanner client-header < protocol/wlr-layer-shell-unstable-v1.xml > src/wlr-layer-shell.h

src/wlr-layer-shell.c : protocol/wlr-layer-shell-unstable-v1.xml
	wayland-scanner private-code < protocol/wlr-layer-shell-unstable-v1.xml > src/wlr-layer-shell.c

src/wlr-virtual-pointer.h : protocol/wlr-virtual-pointer-unstable-v1.xml
	wayland-scanner client-header < protocol/wlr-virtual-pointer-unstable-v1.xml > src/wlr-virtual-pointer.h

src/wlr-virtual-pointer.c : protocol/wlr-virtual-pointer-unstable-v1.xml
	wayland-scanner private-code < protocol/wlr-virtual-pointer-unstable-v1.xml > src/wlr-virtual-pointer.c

src/virtual-keyboard.h : protocol/virtual-keyboard-unstable-v1.xml
	wayland-scanner client-header < protocol/virtual-keyboard-unstable-v1.xml > src/virtual-keyboard.h

src/virtual-keyboard.c : protocol/virtual-keyboard-unstable-v1.xml
	wayland-scanner private-code < protocol/virtual-keyboard-unstable-v1.xml > src/virtual-keyboard.c

MANPAGES := auto-generated-man-pages/kloak.8

man : $(MANPAGES)

auto-generated-man-pages/% : man/%.ronn
	mkdir -p $(@D)
	ronn --manual="kloak Manual" --organization="kloak" <$< >$@

clean :
	rm -f kloak
	rm -f src/xdg-shell-protocol.h src/xdg-shell-protocol.c src/xdg-output-protocol.h src/xdg-output-protocol.c src/wlr-layer-shell.h src/wlr-layer-shell.c src/wlr-virtual-pointer.h src/wlr-virtual-pointer.c src/virtual-keyboard.h src/virtual-keyboard.c
	rm -f auto-generated-man-pages/kloak.8
	rm -f $(DEB_DIR)/amd64/$(BINARY)
	rm -f $(DEB_DIR)/arm64/$(BINARY)
	rm -f $(PKG_MAN)

install : all etc/apparmor.d/usr.bin.kloak $(MANPAGES)
	$(INSTALL) -d -m 755 $(addprefix $(DESTDIR), $(bindir) $(mandir)/man8 $(apparmor_dir))
	$(INSTALL) -m 755 kloak $(DESTDIR)$(bindir)
	$(INSTALL) -m 644 $(MANPAGES) $(DESTDIR)$(mandir)/man8
	$(INSTALL) -m 644 etc/apparmor.d/usr.bin.kloak $(DESTDIR)$(apparmor_dir)

# ---- Publish-toolkit integration (utils/publish) -------------------------
BINARY  := kloak
DEB_DIR := $(HOME)/dev/kloak-ubuntu/deb
PKG_MAN := $(DEB_DIR)/package/usr/share/man/man8/$(BINARY).8.gz

.PHONY: x86_64 aarch64 bootstrap

x86_64:
	$(MAKE) all
	$(MAKE) man
	mkdir -p $(DEB_DIR)/amd64 $(dir $(PKG_MAN))
	install -m 0755 $(BINARY) $(DEB_DIR)/amd64/$(BINARY)
	install -m 0644 auto-generated-man-pages/$(BINARY).8 $(dir $(PKG_MAN))$(BINARY).8
	gzip -9 -n -f $(dir $(PKG_MAN))$(BINARY).8

aarch64:
	@command -v aarch64-linux-gnu-gcc >/dev/null 2>&1 || { \
	  echo "ERROR: aarch64-linux-gnu-gcc not found. Run: make -C c bootstrap"; \
	  exit 1; \
	}
	@test -f /usr/lib/aarch64-linux-gnu/pkgconfig/libinput.pc || { \
	  echo "ERROR: arm64 dev libs missing (no /usr/lib/aarch64-linux-gnu/pkgconfig/libinput.pc)."; \
	  echo "Run: make -C c bootstrap"; \
	  exit 1; \
	}
	$(MAKE) clean
	$(MAKE) all \
	    CC=aarch64-linux-gnu-gcc \
	    PKG_CONFIG=pkg-config \
	    PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig \
	    PKG_CONFIG_LIBDIR=/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig \
	    PKG_CONFIG_SYSROOT_DIR=/
	mkdir -p $(DEB_DIR)/arm64
	install -m 0755 $(BINARY) $(DEB_DIR)/arm64/$(BINARY)

bootstrap:
	sudo dpkg --add-architecture arm64
	sudo apt-get update
	sudo apt-get install -y \
	    build-essential libevdev-dev libinput-dev libwayland-dev \
	    libxkbcommon-dev libwayland-bin pkg-config ronn \
	    crossbuild-essential-arm64 \
	    libevdev-dev:arm64 libinput-dev:arm64 libwayland-dev:arm64 \
	    libxkbcommon-dev:arm64
