#!/bin/sh
# Build cortex's own executables into a directory `CORTEX_ABIN_DIR` can name.
#
# `/abin` is one read-only disk of guest-native executables, and there is no release of
# them yet — `CORTEX_ABIN_DIR` is how a session gets one at all. What that variable wants
# is a directory of binaries built *for the guest*: linux, the host's architecture, and
# linked against nothing the guest is not guaranteed to have.
#
# # Why a container rather than a cross build
#
# `mem` and `index` open a SQLite that is compiled from source (`rusqlite/bundled`, so that
# FTS5 and `vec0` are there whatever the host ships), and compiling C for
# `*-unknown-linux-musl` needs a C cross-compiler that a macOS host does not have — the
# guest crate gets away with `-C linker=rust-lld` only because it is pure Rust. A linux
# container of the guest's own libc has the compiler already, and is one `docker run`
# rather than a toolchain everyone has to install.
#
# # Why static, and why nothing here asks for it
#
# The default base is an Alpine minirootfs, so musl is what a guest usually has — but a
# session may name any image, and `python:3.13-slim` is glibc. `/abin` is the same disk for
# every session, so its executables cannot depend on which one booted.
#
# A musl *host* target links statically already, which is what this build is, so the
# binaries need no loader and there is nothing to pass. Saying `+crt-static` anyway is not
# a no-op: it applies to host artifacts too, and a proc-macro is a dynamic library — so the
# flag that was meant to harden the two executables instead stops `clap_derive` from
# building at all.
#
# ```sh
# cortex-execs/build-abin.sh
# export CORTEX_ABIN_DIR=$PWD/target/abin
# ```
set -eu

root=$(cd -- "$(dirname -- "$0")/.." && pwd)
out=${1:-$root/target/abin}
# Beside the host's artifacts and not among them: these are a different target triple built
# by a different toolchain, and a shared directory is two fingerprint sets fighting.
builder_target=$root/target/abin-builder

case $(uname -m) in
    arm64 | aarch64) platform=linux/arm64 ;;
    x86_64) platform=linux/amd64 ;;
    # A guest runs the host's architecture, so there is nothing to fall back to.
    *) echo "cortex ships no executables for a $(uname -m) host" >&2 && exit 1 ;;
esac

image=${ABIN_BUILDER_IMAGE:-rust:alpine}

# `musl-dev` for the C headers `libsqlite3-sys` compiles against; the image brings gcc.
# `CARGO_HOME` is a named volume, so a second run costs neither the index nor the downloads.
docker run --rm \
    --platform "$platform" \
    --volume "$root:/src" \
    --volume cortex-abin-cargo:/cargo \
    --env CARGO_HOME=/cargo \
    --env CARGO_TARGET_DIR=/src/target/abin-builder \
    --workdir /src \
    "$image" \
    sh -eu -c 'apk add --no-cache musl-dev >/dev/null &&
        exec cargo build --release -p cortex-exec-mem -p cortex-exec-index'

mkdir -p "$out"
for bin in mem index; do
    install -m 755 "$builder_target/release/$bin" "$out/$bin"
done

echo
echo "/abin built in $out:"
ls -l "$out"
echo
echo "  export CORTEX_ABIN_DIR=$out"
