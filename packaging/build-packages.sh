#!/bin/sh
# Build the distribution packages, from a release build of the workspace.
#
#   packaging/build-packages.sh            # .deb and .rpm for this machine
#   packaging/build-packages.sh --musl     # static, for machines unlike this one
#
# Both packages carry all six binaries, the man page, the CLI descriptors, the
# capability manifests, both sets of service files and `nexmesh-install`.
# Neither ENABLES anything: see packaging/README.md.
set -eu

cd "$(dirname "$0")/.."
TARGET=""
SUFFIX=""
for arg in "$@"; do
    case "$arg" in
        --musl) TARGET="--target x86_64-unknown-linux-musl"; SUFFIX="musl" ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

need() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "missing $1 -- cargo install $2" >&2
        exit 1
    }
}
need cargo-deb cargo-deb
need cargo-generate-rpm cargo-generate-rpm

echo "building the workspace"
# shellcheck disable=SC2086
cargo build --release --workspace $TARGET

# cargo-deb builds only the crate it is given, and the assets here are the
# whole workspace, so the build above is the one that matters and this step
# must not start another.
echo "deb"
# shellcheck disable=SC2086
cargo deb -p nexmesh-ctl --no-build $TARGET

echo "rpm"
# shellcheck disable=SC2086
cargo generate-rpm -p crates/ctl $TARGET

echo
echo "packages:"
ls -la target/debian/*.deb 2>/dev/null || true
ls -la target/generate-rpm/*.rpm 2>/dev/null || true
if [ -n "$SUFFIX" ]; then
    ls -la target/x86_64-unknown-linux-musl/debian/*.deb 2>/dev/null || true
    ls -la target/x86_64-unknown-linux-musl/generate-rpm/*.rpm 2>/dev/null || true
fi
