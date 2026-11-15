#!/bin/sh
# Install nexmesh on this machine.
#
#   curl -fsSL https://raw.githubusercontent.com/nextime/nexmesh/main/packaging/install.sh | sh
#   sh packaging/install.sh --role node --hub wss://hub.example.net/v1/node/ws
#
# What it does, in order, and nothing else:
#
#   1. finds a release tarball for this machine's architecture, or builds from
#      a checkout if that is where it is being run from;
#   2. installs the binaries into /usr/local/bin and the man page beside them;
#   3. installs the CLI descriptors into /etc/nexmesh/clis;
#   4. with --role, installs the service for whichever init this machine
#      actually runs -- systemd units or /etc/init.d scripts -- and enables it
#      at boot.
#
# Static musl binaries, so the only requirement is a Linux kernel: no libc
# version to match, no runtime to install, nothing to compile unless you ask.
set -eu

REPO="${NEXMESH_REPO:-nextime/nexmesh}"
VERSION="${NEXMESH_VERSION:-latest}"
PREFIX="${PREFIX:-/usr/local}"
ROLE=""
HUB=""
USER_NAME=""
DRY=0

usage() {
    cat <<USAGE
usage: install.sh [--role hub|node|coderai] [--hub URL] [--user NAME]
                  [--version TAG] [--prefix DIR] [--dry-run]

  --role     also install and enable the service for that role at boot
  --hub      the hub a node should dial (written into its environment file)
  --user     which login a node runs as (default: the invoking user)
  --version  release tag to fetch; default is the latest
  --prefix   where the binaries go; default /usr/local
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --role) shift; ROLE="${1:-}" ;;
        --role=*) ROLE="${1#--role=}" ;;
        --hub) shift; HUB="${1:-}" ;;
        --hub=*) HUB="${1#--hub=}" ;;
        --user) shift; USER_NAME="${1:-}" ;;
        --user=*) USER_NAME="${1#--user=}" ;;
        --version) shift; VERSION="${1:-latest}" ;;
        --prefix) shift; PREFIX="${1:-/usr/local}" ;;
        --dry-run|-n) DRY=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install.sh: unknown argument '$1'" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

say() { printf 'nexmesh: %s\n' "$*"; }
run() { if [ "$DRY" = 1 ]; then printf '  %s\n' "$*"; else "$@"; fi; }

# Root for the system paths, or sudo where it is available.
SUDO=""
if [ "$(id -u)" != 0 ]; then
    if command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
    else
        say "this needs root for $PREFIX and /etc/nexmesh; re-run as root"
        exit 1
    fi
fi
sudo_run() { if [ "$DRY" = 1 ]; then printf '  %s\n' "$SUDO $*"; else $SUDO "$@"; fi; }

ARCH=$(uname -m)
case "$ARCH" in
    x86_64|amd64) TARGET=x86_64-unknown-linux-musl ;;
    aarch64|arm64) TARGET=aarch64-unknown-linux-musl ;;
    *) say "no prebuilt binaries for $ARCH; build from source (see README)"; exit 1 ;;
esac

HERE=$(cd "$(dirname "$0")/.." 2>/dev/null && pwd || echo "")
BINS="nexmesh nexmesh-hub nexmesh-node nexmesh-mcpd nexmesh-orch nexmesh-coderai"

# A checkout with a release build in it is used as-is; otherwise fetch.
SRC=""
if [ -n "$HERE" ] && [ -x "$HERE/target/$TARGET/release/nexmesh" ]; then
    SRC="$HERE/target/$TARGET/release"
    say "installing from this checkout ($SRC)"
elif [ -n "$HERE" ] && [ -x "$HERE/target/release/nexmesh" ]; then
    SRC="$HERE/target/release"
    say "installing from this checkout ($SRC)"
else
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    if [ "$VERSION" = latest ]; then
        URL="https://github.com/$REPO/releases/latest/download/nexmesh-$TARGET.tar.gz"
    else
        URL="https://github.com/$REPO/releases/download/$VERSION/nexmesh-$TARGET.tar.gz"
    fi
    say "fetching $URL"
    if command -v curl >/dev/null 2>&1; then
        run curl -fsSL "$URL" -o "$TMP/nexmesh.tar.gz"
    elif command -v wget >/dev/null 2>&1; then
        run wget -qO "$TMP/nexmesh.tar.gz" "$URL"
    else
        say "neither curl nor wget here"; exit 1
    fi
    run tar -xzf "$TMP/nexmesh.tar.gz" -C "$TMP"
    SRC="$TMP"
    [ -x "$TMP/nexmesh" ] || SRC="$TMP/nexmesh-$TARGET"
fi

say "installing binaries into $PREFIX/bin"
for b in $BINS; do
    if [ -f "$SRC/$b" ]; then
        sudo_run install -Dm755 "$SRC/$b" "$PREFIX/bin/$b"
    fi
done

# The man page and the CLI descriptors, when this is a checkout or a tarball
# that carries them.
for d in "$HERE" "$SRC"; do
    [ -n "$d" ] || continue
    if [ -f "$d/docs/man/nexmesh.1" ]; then
        sudo_run install -Dm644 "$d/docs/man/nexmesh.1" "$PREFIX/share/man/man1/nexmesh.1"
    fi
    if [ -d "$d/clis" ]; then
        sudo_run mkdir -p /etc/nexmesh/clis
        for f in "$d"/clis/*.toml; do
            [ -f "$f" ] || continue
            sudo_run install -Dm644 "$f" "/etc/nexmesh/clis/$(basename "$f")"
        done
    fi
    if [ -d "$d/capabilities" ]; then
        sudo_run mkdir -p /etc/nexmesh/capabilities
        for f in "$d"/capabilities/*.toml; do
            [ -f "$f" ] || continue
            sudo_run install -Dm644 "$f" "/etc/nexmesh/capabilities/$(basename "$f")"
        done
    fi
done

if command -v mandb >/dev/null 2>&1; then
    run $SUDO mandb -q 2>/dev/null || true
fi

say "installed: $(ls "$PREFIX/bin" 2>/dev/null | grep -c '^nexmesh' || echo 0) binaries"

if [ -n "$ROLE" ]; then
    SERVICE=""
    for d in "$HERE" "$SRC"; do
        [ -n "$d" ] || continue
        [ -f "$d/deploy/install-service.sh" ] && SERVICE="$d/deploy/install-service.sh" && break
    done
    if [ -z "$SERVICE" ]; then
        say "no deploy/install-service.sh here; install the service by hand"
        exit 0
    fi
    set -- "$ROLE"
    [ -n "$USER_NAME" ] && set -- "$@" --user "$USER_NAME"
    [ "$DRY" = 1 ] && set -- "$@" --dry-run
    say "installing the $ROLE service, and enabling it at boot"
    $SUDO sh "$SERVICE" "$@"
    if [ -n "$HUB" ] && [ "$ROLE" = node ] && [ "$DRY" = 0 ]; then
        ENVFILE="/etc/nexmesh/node-${USER_NAME:-${SUDO_USER:-$(id -un)}}.env"
        $SUDO sh -c "sed -i 's|^NEXMESH_HUB=.*|NEXMESH_HUB=$HUB|' '$ENVFILE'" || true
        say "wrote the hub address into $ENVFILE"
    fi
fi

cat <<'NEXT'

Next:
  nexmesh --help            what it can do, with examples
  man nexmesh               every argument
  nexmesh                   the operator console, once a hub is reachable

A node needs a token from its hub:  nexmesh-hub enrol <this machine's name>
NEXT
