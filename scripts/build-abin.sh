#!/usr/bin/env bash
# Build cortex's own guest executables into a directory `CORTEX_ABIN_DIR` can name.
#
#   scripts/build-abin.sh
#       This host's architecture, release, into the `abin` directory every session here
#       mounts: `$CORTEX_UVM_HOME/abin`, or `~/.cache/cortex/abin` when that is unset.
#   scripts/build-abin.sh --zig
#       The same, built on this host instead of in a container.
#   scripts/build-abin.sh --arch x86_64
#       The other architecture, to check it still compiles.
#   scripts/build-abin.sh --out /somewhere/else
#       Outside the cache, for a build to inspect rather than run.
#   scripts/build-abin.sh --debug
#       The debug profile: a fraction of the compile time, executables several times the size.
#       `/abin` is mounted, not copied, so the size costs only host disk.
#
# Output is flat (`mem`, `index` and `docread` at the top level, nothing around them) because
# the guest mounts one directory at `/abin` and prepends it to `PATH`.
#
# It goes straight into the cache, not `target/`, because that directory *is* the host's
# `/abin`: a session mounts it if present and boots without it otherwise, with nothing to export
# or copy. So a rebuild changes the executables under running sessions, which is intended.
#
# # Static, and why nothing here asks for it
#
# A session's base image may be musl (an Alpine minirootfs) or glibc (`python:3.13-slim`), and
# `/abin` is the same for every session, so these are static rather than linked against a libc
# that may be absent.
#
# A musl host target, which this build is, already links statically, so there is nothing to
# pass. `+crt-static` is not a no-op: it also applies to host artifacts, and a proc-macro is a
# dynamic library, so it stops `clap_derive` from building.
#
# # Why a container, and what `--zig` is for
#
# `mem` and `index` use a SQLite compiled from source (`rusqlite/bundled`, so FTS5 and `vec0`
# exist whatever the guest ships). That needs a C cross-compiler for `*-unknown-linux-musl`,
# and rustup brings only a linker. A Linux container of the guest's libc already has one and
# costs one `docker run`, with nothing to install.
#
# `--zig` builds the same executables on the host instead: no daemon, and no bind mount, which
# on a Mac is most of the cost of an edit-and-rebuild loop.
#
# Both produce the same static ELF from the same source; only the C compiler that built SQLite
# differs.
#
# # This host's architecture by default
#
# A guest runs the host's architecture, so only that build can boot here. `--arch` checks the
# other still compiles; in a container it is emulated and slow, where `--zig` truly
# cross-compiles.
#
# # Why `docread` cannot read PDFs here
#
# Its `pdfium` feature is off, here and in the release: static linking needs a musl
# `libpdfium.a`, which needs a musl sysroot and a C++ toolchain targeting it. So `docread`
# recognizes a PDF and refuses it. `cortex-execs/docread/build.rs` has the reasoning and the
# workarounds.
set -euo pipefail

die() { echo "build-abin: $*" >&2; exit 1; }

root=$(cd -- "$(dirname -- "$0")/.." && pwd)

# What `/abin` holds, as `<package>:<program>` (crate `cortex-exec-mem` installs as `mem`):
# the package to build, the program to install.
#
# The sole statement of what `/abin` holds. A release publisher must hold the same list, or
# `/abin` differs between the development machine and the one it runs on.
PROGRAMS=(cortex-exec-mem:mem cortex-exec-index:index cortex-exec-docread:docread)

# The container build's image. `rust:alpine` is musl's own toolchain with Rust, so the build
# inside is native and needs no cross-compiler.
IMAGE="${ABIN_BUILDER_IMAGE:-rust:alpine}"

# `<arch>` is spelled as `std::env::consts::ARCH`, which the console server builds its release
# URL from.
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
# Resolved as the host resolves it (`CORTEX_UVM_HOME`, else `~/.cache/cortex`); any other rule
# would fill a directory nothing mounts.
[ -n "$OUT" ] || OUT="${CORTEX_UVM_HOME:-$HOME/.cache/cortex}/abin"
TRIPLE="$(triple_for "$ARCH")"

# Check prerequisites before compiling, as readable errors naming the fix rather than tool
# failures mid-build.
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

# Where cargo actually writes: CARGO_TARGET_DIR is set in most CI and shared-cache setups,
# where a hardcoded `target` would fail at `install` with an uninformative missing path.
TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"

packages=()
for pair in "${PROGRAMS[@]}"; do packages+=(-p "${pair%%:*}"); done

echo "build-abin: building $TRIPLE ($PROFILE)" >&2

if [ "$ZIG" = 1 ]; then
  built="$TARGET_DIR/$TRIPLE/$PROFILE"
  if [ "$PROFILE" = release ]; then
    cargo zigbuild "${packages[@]}" --release --target "$TRIPLE"
  else
    cargo zigbuild "${packages[@]}" --target "$TRIPLE"
  fi
else
  # Separate from the host's artifacts, and per architecture: each container build is a
  # different native toolchain, and sharing a directory would make their fingerprints fight.
  builder="$TARGET_DIR/abin-builder-$ARCH"
  # Native inside the container, so output is under the profile, not a triple.
  built="$builder/$PROFILE"

  if [ "$ARCH" != "$HOST" ]; then
    echo "build-abin: warning: $ARCH is emulated on a $HOST host and will be slow (--zig cross-compiles)" >&2
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

# Anything else here lands on `PATH` ahead of the image's executables once mounted, so a
# program dropped from `/abin` would keep shadowing one in every session. Warned about, not
# deleted: the directory was named by the user, and whatever else it holds is theirs.
for entry in "$OUT"/*; do
  [ -e "$entry" ] || continue
  name="$(basename "$entry")"
  for known in "${names[@]}"; do
    if [ "$name" = "$known" ]; then
      continue 2
    fi
  done
  echo "build-abin: warning: $OUT holds $name, which cortex did not build" >&2
done

echo
echo "/abin built in $OUT:"
ls -l "$OUT"
