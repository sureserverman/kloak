#!/bin/bash
# Selectively pull upstream Whonix/kloak changes into our restructured layout.
#
# Upstream lays files at the repo root (src/, man/, Makefile,
# etc/apparmor.d/, usr/lib/systemd/system/, debian/...).
# Our fork stages source under c/ and packaging assets under deb/package/.
# Upstream's Wayland-specific files (protocol/, usr/libexec/kloak/) are not
# synced: we re-inject input via /dev/uinput instead of wlroots protocols,
# so the compositor-finder helper and protocol XMLs don't exist in our tree.
# This script copies only the subset we use into the right local destination.
#
# Usage:
#   ./sync-upstream.sh             # check: list pending changes, no writes
#   ./sync-upstream.sh pull        # apply: copy files, 3-way merge Makefile
#   ./sync-upstream.sh init        # mark current upstream HEAD as synced
#
# State: .upstream-sync at repo root holds the SHA of the last-applied upstream
# commit; tracked in git so subsequent runs know what to diff against.

set -euo pipefail

UPSTREAM_REMOTE="upstream"
UPSTREAM_URL="https://github.com/Whonix/kloak"
UPSTREAM_BRANCH="master"
UPSTREAM_REF="$UPSTREAM_REMOTE/$UPSTREAM_BRANCH"
SYNC_FILE=".upstream-sync"

# upstream path  ->  local destination
declare -A MAP=(
  [src]="c/src"
  [man]="c/man"
  [etc/apparmor.d]="deb/package/etc/apparmor.d"
  [usr/lib/systemd/system]="deb/package/usr/lib/systemd/system"
)

# Files that have local additions and need a real 3-way merge (upstream:local).
MERGE_FILES=(
  "Makefile:c/Makefile"
)

# Files printed for manual reference (e.g. Version bump in debian/changelog
# needs to be mirrored to deb/package/DEBIAN/control).
INFO_FILES=(
  "debian/changelog"
)

cmd=${1:-check}

cd "$(git rev-parse --show-toplevel)"

# Ensure the upstream remote exists and points where we expect. We deliberately
# don't reuse `origin` because once you push this fork to your own GitHub,
# origin will be your fork, not Whonix.
if ! git remote get-url "$UPSTREAM_REMOTE" >/dev/null 2>&1; then
  echo "==> Adding remote '$UPSTREAM_REMOTE' -> $UPSTREAM_URL"
  git remote add "$UPSTREAM_REMOTE" "$UPSTREAM_URL"
else
  current_url=$(git remote get-url "$UPSTREAM_REMOTE")
  if [[ "$current_url" != "$UPSTREAM_URL" ]]; then
    echo "WARNING: remote '$UPSTREAM_REMOTE' is $current_url (expected $UPSTREAM_URL)"
  fi
fi

git fetch "$UPSTREAM_REMOTE" "$UPSTREAM_BRANCH" >/dev/null 2>&1
upstream_head=$(git rev-parse "$UPSTREAM_REF")

if [[ "$cmd" == "init" ]]; then
  echo "$upstream_head" > "$SYNC_FILE"
  echo "Marked $upstream_head as last-synced upstream commit."
  exit 0
fi

if [[ ! -f "$SYNC_FILE" ]]; then
  echo "ERROR: $SYNC_FILE missing. Run '$0 init' to bootstrap (assumes current"
  echo "       tree mirrors $UPSTREAM_REF)."
  exit 1
fi

last_synced=$(cat "$SYNC_FILE")

if [[ "$last_synced" == "$upstream_head" ]]; then
  echo "Up to date with $UPSTREAM_REF ($upstream_head)"
  exit 0
fi

echo "==> Upstream commits since last sync:"
git --no-pager log --oneline "$last_synced..$upstream_head"
echo

mapfile -t changed < <(git diff --name-only "$last_synced..$upstream_head")

translate () {
  local upath="$1" k
  for k in "${!MAP[@]}"; do
    if [[ "$upath" == "$k" || "$upath" == "$k"/* ]]; then
      printf '%s' "${MAP[$k]}${upath#$k}"
      return 0
    fi
  done
  return 1
}

declare -a copies=() merges=() infos=() ignored=()
for f in "${changed[@]}"; do
  if dest=$(translate "$f"); then
    copies+=("$f|$dest")
    continue
  fi
  matched=0
  for spec in "${MERGE_FILES[@]}"; do
    if [[ "$f" == "${spec%%:*}" ]]; then merges+=("$spec"); matched=1; break; fi
  done
  [[ $matched -eq 1 ]] && continue
  for i in "${INFO_FILES[@]}"; do
    if [[ "$f" == "$i" ]]; then infos+=("$i"); matched=1; break; fi
  done
  [[ $matched -eq 1 ]] && continue
  ignored+=("$f")
done

[[ ${#copies[@]}  -gt 0 ]] && { echo "==> Copy (upstream -> local):";  for c in "${copies[@]}";  do echo "    ${c%%|*}  ->  ${c##*|}"; done; }
[[ ${#merges[@]}  -gt 0 ]] && { echo "==> 3-way merge:";                for m in "${merges[@]}";  do echo "    ${m%%:*}  ->  ${m##*:}"; done; }
[[ ${#infos[@]}   -gt 0 ]] && { echo "==> Info (read, not written):";   for i in "${infos[@]}";   do echo "    $i"; done; }
[[ ${#ignored[@]} -gt 0 ]] && { echo "==> Ignored (outside our subset):"; for i in "${ignored[@]}"; do echo "    $i"; done; }

if [[ "$cmd" != "pull" ]]; then
  echo
  echo "Re-run with 'pull' to apply."
  exit 0
fi

echo
echo "==> Applying copies"
for c in "${copies[@]}"; do
  src="${c%%|*}"; dst="${c##*|}"
  mkdir -p "$(dirname "$dst")"
  if git cat-file -e "$upstream_head:$src" 2>/dev/null; then
    git show "$upstream_head:$src" > "$dst"
    echo "  wrote   $dst"
  else
    rm -f "$dst"
    echo "  removed $dst (deleted upstream)"
  fi
done

conflicts=0
if [[ ${#merges[@]} -gt 0 ]]; then
  echo "==> 3-way merging"
  for spec in "${merges[@]}"; do
    src="${spec%%:*}"; dst="${spec##*:}"
    base=$(mktemp); remote=$(mktemp)
    git show "$last_synced:$src"   > "$base"   2>/dev/null || : > "$base"
    git show "$upstream_head:$src" > "$remote"
    if git merge-file -L local -L base -L upstream "$dst" "$base" "$remote"; then
      echo "  merged  $dst"
    else
      echo "  CONFLICT in $dst — resolve markers manually"
      conflicts=$((conflicts + 1))
    fi
    rm -f "$base" "$remote"
  done
fi

if [[ ${#infos[@]} -gt 0 ]]; then
  echo
  echo "==> Info files"
  for i in "${infos[@]}"; do
    echo "--- $i (head 15 lines) ---"
    git show "$upstream_head:$i" | head -15
    echo
  done
  echo "Reminder: if debian/changelog has a new Version, mirror it to"
  echo "          deb/package/DEBIAN/control before publishing."
fi

if [[ $conflicts -gt 0 ]]; then
  echo
  echo "ERROR: $conflicts merge conflict(s). Resolve, re-run; the script will"
  echo "       see no further upstream diff and bump $SYNC_FILE on success."
  exit 1
fi

echo "$upstream_head" > "$SYNC_FILE"
echo
echo "==> Done. $SYNC_FILE -> $upstream_head"
echo "    Review:  git diff"
echo "    Build:   cd ~/dev/utils && ./publish kloak-ubuntu"
