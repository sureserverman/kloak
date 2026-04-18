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

.PHONY: uinput-smoke
uinput-smoke : src/uinput_smoke.c src/uinput.c src/uinput.h
	$(CC) -Wall -Wextra -O2 -g -Isrc src/uinput_smoke.c src/uinput.c -o uinput-smoke

kloak : src/kloak.c src/uinput.c src/uinput.h
	## Comment out the below line and uncomment the line after it to do a test build with -fanalyzer
	$(CC) -g src/kloak.c src/uinput.c -o kloak -lm -lrt $(shell $(PKG_CONFIG) --cflags --libs libinput) $(shell $(PKG_CONFIG) --cflags --libs libevdev) $(CFLAGS) $(LDFLAGS)
	#$(CC) -g src/kloak.c src/uinput.c -o kloak -lm -lrt $(shell $(PKG_CONFIG) --cflags --libs libinput) $(shell $(PKG_CONFIG) --cflags --libs libevdev) -fanalyzer

MANPAGES := auto-generated-man-pages/kloak.8

man : $(MANPAGES)

auto-generated-man-pages/% : man/%.ronn
	mkdir -p $(@D)
	ronn --manual="kloak Manual" --organization="kloak" <$< >$@

clean :
	rm -f kloak uinput-smoke
	rm -f auto-generated-man-pages/kloak.8

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
	$(MAKE) clean
	$(MAKE) all
	$(MAKE) man
	mkdir -p $(DEB_DIR)/amd64 $(dir $(PKG_MAN))
	install -m 0755 $(BINARY) $(DEB_DIR)/amd64/$(BINARY)
	install -m 0644 auto-generated-man-pages/$(BINARY).8 $(dir $(PKG_MAN))$(BINARY).8
	gzip -9 -n -f $(dir $(PKG_MAN))$(BINARY).8

SYSROOT_ARM64 := $(CURDIR)/sysroot-arm64/root

aarch64:
	@command -v aarch64-linux-gnu-gcc >/dev/null 2>&1 || { \
	  echo "ERROR: aarch64-linux-gnu-gcc not found. Run: make -C c bootstrap"; \
	  exit 1; \
	}
	@test -f $(SYSROOT_ARM64)/usr/lib/aarch64-linux-gnu/pkgconfig/libinput.pc || \
	    $(CURDIR)/build-arm64-sysroot.sh
	$(MAKE) clean
	$(MAKE) all \
	    CC=aarch64-linux-gnu-gcc \
	    PKG_CONFIG=pkg-config \
	    PKG_CONFIG_PATH= \
	    PKG_CONFIG_LIBDIR=$(SYSROOT_ARM64)/usr/lib/aarch64-linux-gnu/pkgconfig:$(SYSROOT_ARM64)/usr/share/pkgconfig \
	    PKG_CONFIG_SYSROOT_DIR=$(SYSROOT_ARM64) \
	    LDFLAGS='-L$(SYSROOT_ARM64)/usr/lib/aarch64-linux-gnu -L$(SYSROOT_ARM64)/lib/aarch64-linux-gnu -Wl,-rpath-link,$(SYSROOT_ARM64)/usr/lib/aarch64-linux-gnu -Wl,-rpath-link,$(SYSROOT_ARM64)/lib/aarch64-linux-gnu -Wl,--allow-shlib-undefined'
	mkdir -p $(DEB_DIR)/arm64
	install -m 0755 $(BINARY) $(DEB_DIR)/arm64/$(BINARY)

# Self-contained bootstrap: install host-side cross toolchain (amd64 packages
# only — no `dpkg --add-architecture`, no system arm64 apt sources). The arm64
# dev libraries are fetched from ports.ubuntu.com into a project-local sysroot
# by build-arm64-sysroot.sh, invoked automatically by the aarch64 target.
bootstrap:
	sudo apt-get install -y \
	    build-essential libevdev-dev libinput-dev pkg-config ronn \
	    crossbuild-essential-arm64
