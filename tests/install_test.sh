#!/usr/bin/env bash
# End-to-end tests for install.sh. Run from anywhere: tests/install_test.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
export VERT_BIN_DIR="$WORK/bin" VERT_SRC_DIR="$WORK/data/src"

pass=0
check() {
    local name="$1" pattern="$2"
    shift 2
    local out
    out="$("$@" 2>&1)" || {
        printf 'FAIL %s: command failed\n%s\n' "$name" "$out"
        exit 1
    }
    if ! grep -q -- "$pattern" <<<"$out"; then
        printf 'FAIL %s: expected /%s/ in:\n%s\n' "$name" "$pattern" "$out"
        exit 1
    fi
    printf 'ok   %s\n' "$name"
    pass=$((pass + 1))
}

fails() {
    local name="$1" pattern="$2"
    shift 2
    local out
    if out="$("$@" 2>&1)"; then
        printf 'FAIL %s: expected failure\n%s\n' "$name" "$out"
        exit 1
    fi
    grep -q -- "$pattern" <<<"$out" || {
        printf 'FAIL %s: expected /%s/ in:\n%s\n' "$name" "$pattern" "$out"
        exit 1
    }
    printf 'ok   %s\n' "$name"
    pass=$((pass + 1))
}

install=("$ROOT/install.sh")

check "help" "Usage: install.sh" "${install[@]}" --help
fails "unknown option" "unknown option" "${install[@]}" --nope
fails "missing value" "needs a value" "${install[@]}" --ref
fails "bad source" "not a vert checkout" "${install[@]}" --source "$WORK"

# Install from this checkout, then re-run: nothing changes.
check "fresh install" "Installed vert" "${install[@]}" --source "$ROOT"
[ -x "$VERT_BIN_DIR/vert" ] || { echo "FAIL binary missing"; exit 1; }
check "binary runs" "^vert " "$VERT_BIN_DIR/vert" --version
check "up to date" "already up to date" "${install[@]}" --source "$ROOT"
check "path warning" "not on your PATH" "${install[@]}" --source "$ROOT"

# A different binary on disk is replaced and reported as an update.
printf '#!/bin/sh\necho old\n' >"$VERT_BIN_DIR/vert"
check "update" "Updated vert" "${install[@]}" --source "$ROOT"
cmp -s "$VERT_BIN_DIR/vert" "$ROOT/target/release/vert" || { echo "FAIL not replaced"; exit 1; }
[ -z "$(find "$VERT_BIN_DIR" -name '.vert.*')" ] || { echo "FAIL temp file left behind"; exit 1; }

# Managed checkout: clone from a local copy of this repository, then update it.
REMOTE="$WORK/remote"
git clone --quiet --bare "$ROOT" "$REMOTE"
branch="$(git -C "$ROOT" rev-parse --abbrev-ref HEAD)"
export VERT_REPO_URL="file://$REMOTE" VERT_REF="$branch"
rm -f "$VERT_BIN_DIR/vert"
check "clone install" "Cloning" "${install[@]}"
[ -d "$VERT_SRC_DIR/.git" ] || { echo "FAIL no managed checkout"; exit 1; }
check "fetch update" "Fetching" "${install[@]}"

mkdir -p "$WORK/not-git"
fails "refuses foreign dir" "not a git checkout" env VERT_SRC_DIR="$WORK/not-git" "${install[@]}"

check "uninstall" "Removed" "${install[@]}" --uninstall
[ ! -e "$VERT_BIN_DIR/vert" ] && [ ! -e "$VERT_SRC_DIR" ] || { echo "FAIL leftovers"; exit 1; }
check "uninstall again" "Nothing to remove" "${install[@]}" --uninstall

printf '\n%d checks passed\n' "$pass"
