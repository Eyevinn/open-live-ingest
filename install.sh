#!/bin/sh
#
# Installs open-live-ingest, and the Strom release it is built against, from the
# GitHub releases. Runs without prompting, so it suits both a terminal and a script.
#
#   curl -fsSL https://raw.githubusercontent.com/Eyevinn/open-live-ingest/main/install.sh | sh
#
# Environment:
#   INSTALL_DIR     where the binaries go (default: /usr/local/bin if writable, else ~/.local/bin)
#   VERSION         open-live-ingest release tag to install (default: latest)
#   SKIP_STROM      set to "true" to leave Strom alone
#   INSTALL_STROM   set to "always" to (re)install Strom even if one is already on PATH
#   GSTREAMER_INSTALL_TYPE  passed to Strom's installer: "minimal" or "full" (default: full)
#
# Strom is installed with its own installer, pinned to STROM_VERSION below. That tag
# must match the strom-types pin in Cargo.toml; a test in tests/pins.rs checks it.

set -eu

REPO="Eyevinn/open-live-ingest"
BIN="open-live-ingest"
STROM_VERSION="v0.6.8"
STROM_INSTALLER="https://raw.githubusercontent.com/Eyevinn/strom/main/install.sh"

VERSION="${VERSION:-latest}"
SKIP_STROM="${SKIP_STROM:-false}"
INSTALL_STROM="${INSTALL_STROM:-missing}"

say() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "$1 is required"
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Linux)  os_part="unknown-linux-musl" ;;
        Darwin) os_part="apple-darwin" ;;
        *) die "unsupported operating system: $os" ;;
    esac
    case "$arch" in
        x86_64|amd64)  arch_part="x86_64" ;;
        aarch64|arm64) arch_part="aarch64" ;;
        *) die "unsupported architecture: $arch" ;;
    esac
    echo "$arch_part-$os_part"
}

pick_install_dir() {
    if [ -n "${INSTALL_DIR:-}" ]; then
        echo "$INSTALL_DIR"
    elif [ -w /usr/local/bin ]; then
        echo /usr/local/bin
    else
        echo "$HOME/.local/bin"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

install_gateway() {
    target=$(detect_target)
    asset="$BIN-$target.tar.gz"
    if [ "$VERSION" = "latest" ]; then
        base="https://github.com/$REPO/releases/latest/download"
    else
        base="https://github.com/$REPO/releases/download/$VERSION"
    fi

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    say "Downloading $asset ($VERSION)"
    curl -fsSL -o "$tmp/$asset" "$base/$asset" \
        || die "no release asset $asset at $base. Is there a release for this platform?"
    curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "could not download SHA256SUMS"

    want=$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
    have=$(sha256 "$tmp/$asset")
    [ -n "$want" ] || die "$asset is not listed in SHA256SUMS"
    [ "$want" = "$have" ] || die "checksum mismatch for $asset"

    tar xzf "$tmp/$asset" -C "$tmp"
    dir=$(pick_install_dir)
    mkdir -p "$dir"
    install -m 0755 "$tmp/$BIN-$target/$BIN" "$dir/$BIN"
    say "Installed $dir/$BIN ($("$dir/$BIN" --version))"

    case ":$PATH:" in
        *":$dir:"*) ;;
        *) say "Note: $dir is not on your PATH" ;;
    esac
}

install_strom() {
    if [ "$SKIP_STROM" = "true" ]; then
        say "Skipping Strom (SKIP_STROM=true)"
        return
    fi
    if [ "$INSTALL_STROM" != "always" ] && command -v strom >/dev/null 2>&1; then
        say "Strom already installed at $(command -v strom); leaving it alone (INSTALL_STROM=always to replace)"
        return
    fi
    say "Installing Strom $STROM_VERSION with its own installer"
    need bash
    curl -fsSL "$STROM_INSTALLER" | \
        AUTO_INSTALL=true VERSION="$STROM_VERSION" SKIP_GRAPHVIZ=true \
        INSTALL_DIR="$(pick_install_dir)" \
        GSTREAMER_INSTALL_TYPE="${GSTREAMER_INSTALL_TYPE:-full}" bash
}

need curl
need tar
install_gateway
install_strom
say "Done. Next: $BIN setup, then $BIN up"
