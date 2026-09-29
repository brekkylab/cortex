#!/usr/bin/env bash
# Build cortex's own executables for the guest, into a directory `CORTEX_ABIN_DIR` can name.
#
#   scripts/abin/build.sh
#       This host's architecture, release, into the `abin` directory every session here
#       mounts: `$CORTEX_UVM_HOME/abin`, or `~/.cache/cortex/abin` when that is unset.
#   scripts/abin/build.sh --zig
#       The same, built on this host instead of in a container.
#   scripts/abin/build.sh --arch x86_64
#       The other architecture, to find out whether it still compiles.
#   scripts/abin/build.sh --out /somewhere/else
#       Somewhere other than the cache, for a build to look at rather than to run.
#   scripts/abin/build.sh --debug
#       The debug profile, which compiles in a fraction of the time and produces executables
#       several times the size. `/abin` is mounted rather than copied, so that size is the
#       host's disk and nothing the guest pays for.
#
# What comes out is flat — `mem`, `index` and `docread` at the top level and nothing around
# them — because that is what the guest mounts: one directory at `/abin`, prepended to `PATH`.
#
# It goes straight into the cache rather than into `target/`, because that directory *is* how a
# host has an `/abin`: a session mounts it if it is there and boots without one if it is not,
# and there is no variable to export and nothing to copy. Which also means the executables
# change under a session that is already running, and for a program being rebuilt between
# sessions that is the point.
#
# # Static, and why nothing here asks for it
#
# The base image a session boots is whatever it named: an Alpine minirootfs is musl,
# `python:3.13-slim` is glibc. `/abin` is the same set of executables for every session, so
# what is in it cannot depend on which one booted — which is what makes these static rather
# than linked against a libc that may not be there.
#
# A musl *host* target links statically already, which is what this build is, so the
# executables need no loader and there is nothing to pass. Saying `+crt-static` anyway is not
# a no-op: it applies to host artifacts too, and a proc-macro is a dynamic library — so the
# flag that was meant to harden these three instead stops `clap_derive` from building at all.
#
# # Why a container, and what `--zig` is for
#
# `mem` and `index` open a SQLite compiled from source (`rusqlite/bundled`, so that FTS5 and
# `vec0` are there whatever the guest ships). Compiling C for `*-unknown-linux-musl` needs a C
# cross-compiler, and there is no Rust-only answer to that: rustup brings a linker and no C
# toolchain, so it has to come from somewhere. A Linux container of the guest's own libc has
# one already and is one `docker run` — nothing to install, which is what a build everyone has
# to be able to run wants to cost.
#
# `--zig` is the same three executables built here instead, and it is worth having because it
# builds on the host's own filesystem rather than through a bind mount — which on a Mac is most
# of the difference in an edit-and-rebuild loop — and because it needs no daemon running.
#
# The two produce the same static ELF from the same source. What differs is which C compiler
# compiled SQLite, which is a difference worth knowing about and not one worth choosing
# between on most days.
#
# # This host's architecture by default
#
# A guest runs the host's architecture, so the only build that can actually be booted here is
# this one. `--arch` is for finding out whether the other still compiles — and under a
# container it is emulated and slow, where `--zig` cross-compiles for real.
#
# # Why `docread` cannot read PDFs here
#
# Its `pdfium` feature is off, which is also what the release is built with: linking a PDF
# engine in statically needs a `libpdfium.a` for this target, and building one for musl needs a
# musl sysroot and a C++ toolchain aimed at it. So a PDF is a file `docread` names and refuses.
# `cortex-execs/docread/build.rs` is where that is argued and where the ways around it are.
set -euo pipefail

die() { echo "abin/build: $*" >&2; exit 1; }

root=$(cd -- "$(dirname -- "$0")/../.." && pwd)

# What `/abin` holds, as `<package>:<program>`. The two are not the same string — the crate is
# `cortex-exec-mem` and the name a caller types is `mem` — and both are needed here: one to
# build and one to install.
#
# This is the only statement of what `/abin` holds. Anything that publishes a release has to
# hold the same list, and a program added to one and not the other is an `/abin` that differs
# between the machine it was developed on and the machine it runs on.
PROGRAMS=(cortex-exec-mem:mem cortex-exec-index:index cortex-exec-docread:docread)

# The image the container build runs in. `rust:alpine` is musl's own toolchain with a Rust in
# it, which is the whole reason it is this and not a Debian image with a cross-compiler bolted
# on: the build inside it is native, so nothing is being cross-compiled at all.
IMAGE="${ABIN_BUILDER_IMAGE:-rust:alpine}"

# `<arch>` is what `std::env::consts::ARCH` calls it, which is what the console server builds
# its release URL from and therefore the only spelling worth taking.
triple_for() {
  case "$1" in
    aarch64) echo aarch64-unknown-linux-musl ;;
    x86_64) echo x86_64-unknown-linux-musl ;;
    *) die "'$1' is not an architecture cortex builds for (aarch64, x86_64)" ;;
  esac
}

platform_for() {
  case "$1" in
    aarch64) echo linux/arm64 ;;
    x86_64) echo linux/amd64 ;;
    *) die "'$1' is not an architecture cortex builds for (aarch64, x86_64)" ;;
  esac
}

host_arch() {
  case "$(uname -m)" in
    arm64 | aarch64) echo aarch64 ;;
    x86_64) echo x86_64 ;;
    # A guest runs the host's architecture, so there is nothing to fall back to.
    *) die "cortex builds no executables for a $(uname -m) host" ;;
  esac
}

ARCH=""
OUT=""
PROFILE=release
ZIG=0

while [ $# -gt 0 ]; do
  case "$1" in
    --zig) ZIG=1 ;;
    --arch)
      shift
      [ $# -gt 0 ] || die "--arch needs a value"
      ARCH="$1"
      ;;
    --out)
      shift
      [ $# -gt 0 ] || die "--out needs a value"
      OUT="$1"
      ;;
    --debug) PROFILE=debug ;;
    *) die "unknown argument '$1' (see the header of this script)" ;;
  esac
  shift
done

HOST="$(host_arch)"
[ -n "$ARCH" ] || ARCH="$HOST"
# The same place the host resolves, spelled the same way: `CORTEX_UVM_HOME`, else
# `~/.cache/cortex`. A second rule here would be a directory this fills and nothing mounts.
[ -n "$OUT" ] || OUT="${CORTEX_UVM_HOME:-$HOME/.cache/cortex}/abin"
TRIPLE="$(triple_for "$ARCH")"

# Prerequisites, said as sentences rather than left to fail as tool errors — and said before
# anything is compiled, so a missing target is a line to run rather than a surprise after the
# first crate.
if [ "$ZIG" = 1 ]; then
  command -v zig >/dev/null || die "zig is not on PATH (brew install zig)"
  command -v cargo-zigbuild >/dev/null \
    || die "cargo-zigbuild is not installed (cargo install cargo-zigbuild)"
  rustup target list --installed | grep -qx "$TRIPLE" \
    || die "missing target $TRIPLE (rustup target add $TRIPLE)"
else
  command -v docker >/dev/null || die "docker is not on PATH (or build with --zig)"
  docker info >/dev/null 2>&1 || die "docker is installed but not running"
fi

# Where cargo actually writes. Hardcoding `target` is wrong wherever CARGO_TARGET_DIR is set,
# which is most CI and any shared-cache setup — and it would fail at the install below with a
# missing path rather than anywhere informative.
TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"

packages=()
for pair in "${PROGRAMS[@]}"; do packages+=(-p "${pair%%:*}"); done

echo "abin/build: building $TRIPLE ($PROFILE)" >&2

if [ "$ZIG" = 1 ]; then
  built="$TARGET_DIR/$TRIPLE/$PROFILE"
  if [ "$PROFILE" = release ]; then
    cargo zigbuild "${packages[@]}" --release --target "$TRIPLE"
  else
    cargo zigbuild "${packages[@]}" --target "$TRIPLE"
  fi
else
  # Beside the host's artifacts and not among them: what a container builds is a different
  # toolchain's, and a shared directory is two fingerprint sets fighting. Per architecture for
  # the same reason — inside the container the build is native, so two architectures in one
  # directory would be the same fight again.
  builder="$TARGET_DIR/abin-builder-$ARCH"
  # Native inside the container, so what comes out is under the profile and not under a triple.
  built="$builder/$PROFILE"

  if [ "$ARCH" != "$HOST" ]; then
    echo "abin/build: warning: $ARCH is emulated on a $HOST host and will be slow (--zig cross-compiles)" >&2
  fi

  profile_flag=""
  [ "$PROFILE" = debug ] || profile_flag="--release"

  # `musl-dev` for the C headers `libsqlite3-sys` compiles against; the image brings gcc.
  # `CARGO_HOME` is a named volume, so a second run costs neither the index nor the downloads.
  docker run --rm \
    --platform "$(platform_for "$ARCH")" \
    --volume "$root:/src" \
    --volume "cortex-abin-cargo-$ARCH:/cargo" \
    --env CARGO_HOME=/cargo \
    --env CARGO_TARGET_DIR="/src/target/abin-builder-$ARCH" \
    --workdir /src \
    "$IMAGE" \
    sh -eu -c "apk add --no-cache musl-dev >/dev/null &&
        exec cargo build $profile_flag ${packages[*]}"
fi

mkdir -p "$OUT"
names=()
for pair in "${PROGRAMS[@]}"; do
  program="${pair##*:}"
  install -m 755 "$built/$program" "$OUT/$program"
  names+=("$program")
done

# Anything else in there is on `PATH` ahead of the image's own executables the moment this
# directory is mounted, so a program that used to be in `/abin` and no longer is would go on
# shadowing one in every session started here. Said rather than deleted: this is a directory
# somebody named, and what else they keep in it is theirs.
for entry in "$OUT"/*; do
  [ -e "$entry" ] || continue
  name="$(basename "$entry")"
  for known in "${names[@]}"; do
    if [ "$name" = "$known" ]; then
      continue 2
    fi
  done
  echo "abin/build: warning: $OUT holds $name, which cortex did not build" >&2
done

echo
echo "/abin built in $OUT:"
ls -l "$OUT"
