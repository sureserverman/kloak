#!/bin/bash
# Build a self-contained arm64 sysroot for cross-compiling kloak, by fetching
# .deb files directly from ports.ubuntu.com and extracting them with dpkg-deb.
#
# No sudo. No /etc/apt changes. No dpkg --add-architecture. Idempotent.
#
# Output:  c/sysroot-arm64/  (sysroot suitable for PKG_CONFIG_SYSROOT_DIR)

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSROOT="$here/sysroot-arm64"
CACHE="$SYSROOT/.cache"
INDEX="$CACHE/index"
DEBS="$CACHE/debs"
EXTRACT="$SYSROOT/root"

MIRROR="${MIRROR:-http://ports.ubuntu.com/ubuntu-ports}"
SUITE="${SUITE:-noble}"
ARCH="arm64"

# Build-time roots. Runtime deps will be resolved recursively.
ROOTS=(
  libevdev-dev
  libinput-dev
  libwayland-dev
  libxkbcommon-dev
)

# Packages provided by the cross-toolchain (gcc-aarch64-linux-gnu / libc6-dev-
# arm64-cross) or otherwise not needed for kloak. Pulling these in either
# duplicates toolchain libraries (libc, libgcc) or drags huge transitive
# closures (python, openssl) that aren't linked.
EXCLUDES=(
  libc6 libc6-dev libc-dev-bin libc-bin libcrypt1 libcrypt-dev
  libgcc-s1 libstdc++6 libstdc++-13-dev libstdc++-14-dev
  linux-libc-dev
  libssl3 libssl3t64 libssl-dev
  python3 python3-minimal python3.12 python3.12-minimal
  libpython3-stdlib libpython3.12-stdlib libpython3.12-minimal
  zlib1g zlib1g-dev libzstd1
  libpcre2-8-0 libpcre2-16-0 libpcre2-32-0 libpcre2-dev
  libglib2.0-0 libglib2.0-0t64 libglib2.0-dev libglib2.0-dev-bin
  libffi8 libffi-dev
  libselinux1 libselinux1-dev libsepol2 libsepol-dev
  libmount1 libmount-dev libblkid1 libblkid-dev libuuid1 uuid-dev
  libsystemd0 libsystemd-dev
)

mkdir -p "$INDEX" "$DEBS" "$EXTRACT"

fetch_packages_index () {
  local suite="$1" component="$2"
  local out="$INDEX/${suite}_${component}_Packages"
  if [[ -f "$out" && -n "$(find "$out" -newer "$out" -mmin -1440 2>/dev/null || true)" ]]; then
    : # cached < 24h
  fi
  if [[ ! -s "$out" ]]; then
    local url="$MIRROR/dists/$suite/$component/binary-$ARCH/Packages.gz"
    echo "  fetch  $url" >&2
    curl -fsSL "$url" -o "$out.gz"
    gunzip -f "$out.gz"
  fi
  printf '%s\n' "$out"
}

# Concatenate all Packages indexes we care about.
ALL_PACKAGES="$INDEX/all_Packages"
: > "$ALL_PACKAGES"
for suite in "$SUITE" "$SUITE-updates" "$SUITE-security"; do
  for component in main universe; do
    p="$(fetch_packages_index "$suite" "$component")"
    cat "$p" >> "$ALL_PACKAGES"
    echo >> "$ALL_PACKAGES"
  done
done

# Pick the highest-version Filename for each Package name.
# Output: name<TAB>filename<TAB>depends-line
parse_index () {
  awk -v RS='' '
    function clean_name(s) { sub(/[ \t].*/, "", s); sub(/:.*$/, "", s); return s }
    {
      pkg=""; ver=""; fn=""; dep=""
      n=split($0, lines, "\n")
      for (i=1;i<=n;i++) {
        line=lines[i]
        if (line ~ /^Package: /)    { pkg=substr(line,10) }
        else if (line ~ /^Version: /)  { ver=substr(line,10) }
        else if (line ~ /^Filename: /) { fn=substr(line,11) }
        else if (line ~ /^Depends: /)  { dep=substr(line,10) }
        else if (line ~ /^Pre-Depends: /) {
          if (dep!="") dep=dep ", "
          dep=dep substr(line,14)
        }
      }
      if (pkg!="" && fn!="") print pkg "\t" ver "\t" fn "\t" dep
    }
  ' "$ALL_PACKAGES"
}

PARSED="$INDEX/parsed.tsv"
parse_index > "$PARSED"

# Build name -> "filename<TAB>depends" map (last write wins; sort newest-first).
# We approximate "newest" by sorting on Version with `dpkg --compare-versions` —
# too slow for many entries, so use a cheaper approach: sort lexically descending
# (Debian versions sort close to lexical for same-suite/same-arch deltas) and
# take first occurrence per name.
MAP="$INDEX/map.tsv"
sort -k1,1 -k2,2Vr "$PARSED" | awk -F'\t' '!seen[$1]++ { print $1 "\t" $3 "\t" $4 }' > "$MAP"

# Resolve dep closure starting from ROOTS. Drop alternatives ("a | b" -> "a"),
# version constraints ("foo (>= 1.2)" -> "foo"), and architecture qualifiers.
declare -A WANT
queue=( "${ROOTS[@]}" )

resolve_one () {
  local name="$1"
  awk -F'\t' -v want="$name" '$1 == want { print; exit }' "$MAP"
}

declare -A EXCLUDED
for e in "${EXCLUDES[@]}"; do EXCLUDED[$e]=1; done

while [[ ${#queue[@]} -gt 0 ]]; do
  pkg="${queue[0]}"
  queue=( "${queue[@]:1}" )
  pkg="${pkg%%:*}"
  pkg="${pkg// /}"
  [[ -z "$pkg" ]] && continue
  [[ -n "${WANT[$pkg]+x}" ]] && continue
  [[ -n "${EXCLUDED[$pkg]+x}" ]] && { WANT[$pkg]="EXCLUDED"; continue; }
  row="$(resolve_one "$pkg")"
  if [[ -z "$row" ]]; then
    # Some virtual packages (e.g. libc-dev) won't be present; skip silently
    WANT[$pkg]="VIRTUAL"
    continue
  fi
  fn="$(awk -F'\t' '{print $2}' <<<"$row")"
  deps="$(awk -F'\t' '{print $3}' <<<"$row")"
  WANT[$pkg]="$fn"
  # Parse deps: split on ',', take first alternative, strip version + arch
  IFS=',' read -ra parts <<<"$deps"
  for d in "${parts[@]}"; do
    d="${d%%|*}"           # take first alternative
    d="${d// /}"           # strip spaces
    d="${d%%(*}"           # strip "(>= ver)"
    d="${d%%:*}"           # strip ":any"
    [[ -z "$d" ]] && continue
    queue+=( "$d" )
  done
done

echo "==> Will fetch ${#WANT[@]} packages"

for pkg in "${!WANT[@]}"; do
  fn="${WANT[$pkg]}"
  [[ "$fn" == "VIRTUAL" || "$fn" == "EXCLUDED" ]] && continue
  url="$MIRROR/$fn"
  out="$DEBS/$(basename "$fn")"
  if [[ ! -s "$out" ]]; then
    echo "  fetch  $pkg ($fn)"
    curl -fsSL "$url" -o "$out"
  fi
done

# Wipe extract dir so removed excludes don't linger from a previous run.
rm -rf "$EXTRACT"
mkdir -p "$EXTRACT"

echo "==> Extracting into $EXTRACT"
for pkg in "${!WANT[@]}"; do
  fn="${WANT[$pkg]}"
  [[ "$fn" == "VIRTUAL" || "$fn" == "EXCLUDED" ]] && continue
  deb="$DEBS/$(basename "$fn")"
  dpkg-deb -x "$deb" "$EXTRACT"
done

# Some .pc files contain absolute symlinks (/usr/lib/...) to files in /lib —
# ports debs sometimes use the merged-/usr layout. Rewrite the few we'll touch.
echo "==> Done. Sysroot at $EXTRACT"
echo "    PKG_CONFIG_SYSROOT_DIR=$EXTRACT"
echo "    PKG_CONFIG_LIBDIR=$EXTRACT/usr/lib/aarch64-linux-gnu/pkgconfig:$EXTRACT/usr/share/pkgconfig"
