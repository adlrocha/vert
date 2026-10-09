#!/usr/bin/env bash
# Install or update vert.
#
#   curl -fsSL https://raw.githubusercontent.com/adlrocha/vert/main/install.sh | bash
#
# Re-running the installer updates vert in place. It fetches the latest source,
# rebuilds, and replaces the binary only when it changed. Run with --help for options.
set -euo pipefail

REPO_URL="${VERT_REPO_URL:-https://github.com/adlrocha/vert.git}"
REF="${VERT_REF:-main}"
BIN_DIR="${VERT_BIN_DIR:-$HOME/.local/bin}"
SRC_DIR="${VERT_SRC_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/vert/src}"
SOURCE=""
UNINSTALL=0

if [ -t 1 ]; then
    BLUE=$'\033[34m' GREEN=$'\033[32m' YELLOW=$'\033[33m' RED=$'\033[31m' DIM=$'\033[2m' RESET=$'\033[0m'
else
    BLUE="" GREEN="" YELLOW="" RED="" DIM="" RESET=""
fi

say() { printf '%s◆%s %s\n' "$BLUE" "$RESET" "$*"; }
ok() { printf '%s✓%s %s\n' "$GREEN" "$RESET" "$*"; }
warn() { printf '%s!%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die() {
    printf '%serror:%s %s\n' "$RED" "$RESET" "$*" >&2
    exit 1
}

usage() {
    cat <<EOF
Install or update vert, a terminal UI for following changes in a git repository.

Usage: install.sh [options]

Options:
  --bin-dir DIR   Where to put the binary   (default: \$HOME/.local/bin, env VERT_BIN_DIR)
  --ref REF       Branch or tag to build    (default: main, env VERT_REF)
  --source DIR    Build an existing checkout instead of fetching
  --uninstall     Remove the binary and the managed source checkout
  -h, --help      Show this help

Environment: VERT_REPO_URL (default $REPO_URL), VERT_SRC_DIR (default $SRC_DIR).
EOF
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "$1 is required. $2"
}

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --bin-dir) [ $# -ge 2 ] || die "--bin-dir needs a value"; BIN_DIR="$2"; shift 2 ;;
            --ref) [ $# -ge 2 ] || die "--ref needs a value"; REF="$2"; shift 2 ;;
            --source) [ $# -ge 2 ] || die "--source needs a value"; SOURCE="$2"; shift 2 ;;
            --uninstall) UNINSTALL=1; shift ;;
            -h | --help) usage; exit 0 ;;
            *) die "unknown option: $1 (see --help)" ;;
        esac
    done
}

is_vert_checkout() {
    [ -f "$1/Cargo.toml" ] && grep -q '^name = "vert"$' "$1/Cargo.toml"
}

# Echoes the directory to build from, fetching or updating the managed checkout.
resolve_source() {
    if [ -n "$SOURCE" ]; then
        is_vert_checkout "$SOURCE" || die "$SOURCE is not a vert checkout"
        (cd "$SOURCE" && pwd)
        return
    fi
    need git "Install it with your package manager (on macOS: xcode-select --install)."
    if [ -d "$SRC_DIR/.git" ]; then
        say "Fetching $REF from $REPO_URL" >&2
        git -C "$SRC_DIR" remote set-url origin "$REPO_URL"
        git -C "$SRC_DIR" fetch --quiet --depth 1 origin "$REF" || die "could not fetch $REF"
        git -C "$SRC_DIR" checkout --quiet --force FETCH_HEAD
    elif [ -e "$SRC_DIR" ]; then
        die "$SRC_DIR exists but is not a git checkout; move it away or set VERT_SRC_DIR"
    else
        say "Cloning $REPO_URL ($REF)" >&2
        mkdir -p "$(dirname "$SRC_DIR")"
        git clone --quiet --depth 1 --branch "$REF" "$REPO_URL" "$SRC_DIR" || die "could not clone $REPO_URL"
    fi
    echo "$SRC_DIR"
}

uninstall() {
    local removed=0
    if [ -e "$BIN_DIR/vert" ]; then
        rm -f "$BIN_DIR/vert"
        ok "Removed $BIN_DIR/vert"
        removed=1
    fi
    if [ -d "$SRC_DIR/.git" ]; then
        rm -rf "$SRC_DIR"
        ok "Removed $SRC_DIR"
        removed=1
    fi
    [ "$removed" -eq 1 ] || say "Nothing to remove"
}

main() {
    parse_args "$@"
    if [ "$UNINSTALL" -eq 1 ]; then
        uninstall
        return
    fi
    need cargo "Install Rust from https://rustup.rs and re-run."

    local src built dest action commit version tmp
    src="$(resolve_source)"
    commit="$(git -C "$src" rev-parse --short HEAD 2>/dev/null || echo "local")"

    say "Building vert ${DIM}($commit)${RESET}, this can take a minute the first time"
    cargo build --release --locked --quiet \
        --manifest-path "$src/Cargo.toml" --target-dir "$src/target" ||
        die "build failed"
    built="$src/target/release/vert"
    dest="$BIN_DIR/vert"

    mkdir -p "$BIN_DIR"
    if [ -x "$dest" ] && cmp -s "$built" "$dest"; then
        ok "vert is already up to date ${DIM}($commit, $dest)${RESET}"
    else
        action="Installed"
        [ -e "$dest" ] && action="Updated"
        # Copy next to the destination and rename, so the swap is atomic and a
        # running vert keeps working.
        tmp="$(mktemp "$BIN_DIR/.vert.XXXXXX")"
        trap 'rm -f "$tmp"' EXIT
        cp "$built" "$tmp"
        chmod 755 "$tmp"
        mv -f "$tmp" "$dest"
        trap - EXIT
        version="$("$dest" --version 2>/dev/null || echo vert)"
        ok "$action $version ${DIM}($commit)${RESET} at $dest"
    fi

    case ":$PATH:" in
        *":$BIN_DIR:"*) ;;
        *) warn "$BIN_DIR is not on your PATH. Add it, e.g.: export PATH=\"$BIN_DIR:\$PATH\"" ;;
    esac
}

# Everything runs from main, so a partially downloaded script never runs halfway.
main "$@"
