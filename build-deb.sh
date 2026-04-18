#!/bin/bash
# Build kloak into a .deb using the project convention:
#   - compile in source tree
#   - stage binary under deb/amd64/
#   - assemble deb/package/ filesystem tree
#   - dpkg-deb --build
#
# Produces ./kloak_<version>_amd64.deb in the project root.

set -euo pipefail

cd "$(dirname "$0")"

PROJECT_ROOT="$(pwd)"
PACKAGE_DIR="${PROJECT_ROOT}/deb/package"
STAGE_DIR="${PROJECT_ROOT}/deb/amd64"

BUILD_DEPS=(
    build-essential
    libevdev-dev
    libinput-dev
    libwayland-dev
    libxkbcommon-dev
    libwayland-bin
    pkg-config
    ronn
)

missing=()
for pkg in "${BUILD_DEPS[@]}"; do
    dpkg -s "$pkg" >/dev/null 2>&1 || missing+=("$pkg")
done
if (( ${#missing[@]} )); then
    echo "Installing missing build dependencies: ${missing[*]}"
    sudo apt-get update
    sudo apt-get install -y "${missing[@]}"
fi

echo "==> Compiling kloak"
make -C "${PROJECT_ROOT}" clean
make -C "${PROJECT_ROOT}"
make -C "${PROJECT_ROOT}" man

echo "==> Staging binary to deb/amd64/"
mkdir -p "${STAGE_DIR}"
cp "${PROJECT_ROOT}/kloak" "${STAGE_DIR}/kloak"

echo "==> Assembling deb/package/ tree"
install -D -m 0755 "${STAGE_DIR}/kloak"                                     "${PACKAGE_DIR}/usr/bin/kloak"
install -D -m 0755 "${PROJECT_ROOT}/usr/libexec/kloak/find_wl_compositor"   "${PACKAGE_DIR}/usr/libexec/kloak/find_wl_compositor"
install -D -m 0644 "${PROJECT_ROOT}/usr/lib/systemd/system/kloak.service"   "${PACKAGE_DIR}/usr/lib/systemd/system/kloak.service"
install -D -m 0644 "${PROJECT_ROOT}/etc/apparmor.d/usr.bin.kloak"           "${PACKAGE_DIR}/etc/apparmor.d/usr.bin.kloak"
install -D -m 0644 "${PROJECT_ROOT}/etc/apparmor.d/usr.libexec.kloak.find_wl_compositor" \
                                                                            "${PACKAGE_DIR}/etc/apparmor.d/usr.libexec.kloak.find_wl_compositor"

install -D -m 0644 "${PROJECT_ROOT}/auto-generated-man-pages/kloak.8"       "${PACKAGE_DIR}/usr/share/man/man8/kloak.8"
gzip -9 -n -f "${PACKAGE_DIR}/usr/share/man/man8/kloak.8"

chmod 0755 "${PACKAGE_DIR}/DEBIAN/postinst" "${PACKAGE_DIR}/DEBIAN/prerm"

VERSION=$(awk '/^Version:/ {print $2}' "${PACKAGE_DIR}/DEBIAN/control")
OUTPUT="${PROJECT_ROOT}/kloak_${VERSION}_amd64.deb"

echo "==> Building .deb"
dpkg-deb --root-owner-group --build "${PACKAGE_DIR}" "${OUTPUT}"

echo
echo "Built: ${OUTPUT}"
dpkg-deb -I "${OUTPUT}" | sed 's/^/  /'
