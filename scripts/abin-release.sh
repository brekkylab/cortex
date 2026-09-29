#!/usr/bin/env bash
# Build cortex's own guest executables and publish them.
#
#   scripts/abin-release.sh
#       Build every OS below, upload, and move `abin/latest` to this commit.
#   scripts/abin-release.sh --dry-run
#       Build and stop.
#   scripts/abin-release.sh --os linux
#       Just that OS's architectures.
#   scripts/abin-release.sh --no-latest-pointer
#       Upload, but leave `abin/latest` naming whatever it named before.
#   scripts/abin-release.sh --latest-pointer-only
#       Move `abin/latest` to this commit and do nothing else.
#
# HEAD's git sha names the release and is its whole identity (nothing downstream verifies the
# bytes), so a dirty tree is refused.
#
# # The latest-pointer, and why moving it is separable
#
# `abin/latest` holds one line, the **latest-pointer**: the sha of the release a session uses
# when told nothing. It must name a *completely* uploaded release. One run just writes it last,
# but in a parallel OS matrix every job would move it and the last to finish would win whether
# or not the others had. So builders run with `--no-latest-pointer`, and one job afterwards
# runs `--latest-pointer-only`. That split is safe because a release the pointer never reaches
# is inert, not broken: its tarballs sit under a sha nothing refers to.
set -euo pipefail

BUCKET="${ABIN_BUCKET:-cortex-dist-044443350235-us-east-1-an}"

die() { echo "abin-release: $*" >&2; exit 1; }

# Every OS this script builds, in build order. Only Linux, since the guest is the only
# consumer. The key layout already carries the OS, so another is a line here plus a CI matrix
# entry.
ALL_OSES=(linux)

# `<arch>:<rust target>` per OS. `<arch>` goes in the filename and matches
# `std::env::consts::ARCH`, which the client builds its URL from.
targets_for() {
  case "$1" in
    linux) echo "aarch64:aarch64-unknown-linux-musl x86_64:x86_64-unknown-linux-musl" ;;
    *) die "no targets are known for '$1' (this script builds: ${ALL_OSES[*]})" ;;
  esac
}

DRY_RUN=0
LATEST_POINTER_ONLY=0
LATEST_POINTER=1
OSES=()

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    --no-latest-pointer) LATEST_POINTER=0 ;;
    --latest-pointer-only) LATEST_POINTER_ONLY=1 ;;
    --os)
      shift
      [ $# -gt 0 ] || die "--os needs a value"
      OSES+=("$1")
      ;;
    *) die "unknown argument '$1' (see the header of this script)" ;;
  esac
  shift
done
[ "$LATEST_POINTER_ONLY" = 0 ] || [ "$LATEST_POINTER" = 1 ] \
  || die "--latest-pointer-only and --no-latest-pointer ask for opposite things"
[ ${#OSES[@]} -gt 0 ] || OSES=("${ALL_OSES[@]}")
# Validate before building, so a typo fails before any architecture is compiled and uploaded.
for os in "${OSES[@]}"; do targets_for "$os" >/dev/null; done

# Credentials come from the aws CLI's usual sources. Default the profile only when neither a
# profile nor env keys are set (a laptop); in CI a named profile that does not exist is an
# error. `--profile` is never passed, since it would override the environment.
if [ -z "${AWS_PROFILE:-}" ] && [ -z "${AWS_ACCESS_KEY_ID:-}" ]; then
  export AWS_PROFILE=brekkylab
fi

# Check only the prerequisites this invocation reaches, as readable errors rather than tool
# failures. Moving the latest-pointer compiles nothing.
[ "$DRY_RUN" = 1 ] || command -v aws >/dev/null || die "the aws CLI is not on PATH"
if [ "$LATEST_POINTER_ONLY" = 0 ]; then
  command -v zig >/dev/null || die "zig is not on PATH (brew install zig)"
  command -v cargo-zigbuild >/dev/null \
    || die "cargo-zigbuild is not installed (cargo install cargo-zigbuild)"
  for os in "${OSES[@]}"; do
    for pair in $(targets_for "$os"); do
      triple="${pair##*:}"
      rustup target list --installed | grep -qx "$triple" \
        || die "missing target $triple (rustup target add $triple)"
    done
  done
fi

# A tarball built from uncommitted changes and filed under this sha would make the path lie,
# undetected downstream.
[ -z "$(git status --porcelain)" ] || die "the working tree is dirty; commit or stash first"

SHA="$(git rev-parse HEAD)"
# A warning and not a refusal: a release can be cut and pushed in either order.
if [ -z "$(git branch -r --contains "$SHA" 2>/dev/null)" ]; then
  echo "abin-release: warning: $SHA is on no remote branch; nobody else can check it out" >&2
fi

OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

move_latest_pointer() {
  printf '%s\n' "$SHA" > "$OUT/latest"
  aws s3 cp --cache-control "no-cache" "$OUT/latest" "s3://$BUCKET/abin/latest"
  echo "abin-release: the latest-pointer is now $SHA" >&2
}

if [ "$LATEST_POINTER_ONLY" = 1 ]; then
  if [ "$DRY_RUN" = 1 ]; then
    echo "abin-release: would move the latest-pointer to $SHA" >&2
    exit 0
  fi
  move_latest_pointer
  exit 0
fi

# Where cargo actually writes: CARGO_TARGET_DIR is set in most CI and shared-cache setups,
# where a hardcoded `target` would fail at `tar` with an uninformative missing path.
TARGET_DIR="${CARGO_TARGET_DIR:-target}"

built=()
for os in "${OSES[@]}"; do
  for pair in $(targets_for "$os"); do
    arch="${pair%%:*}"
    triple="${pair##*:}"
    name="abin-$os-$arch.tar.gz"
    echo "abin-release: building $triple" >&2
    cargo zigbuild -p cortex-exec-mem -p cortex-exec-index --release --target "$triple"
    # Flat: `mem` and `index` at the top level, exactly as /abin holds them.
    tar czf "$OUT/$name" -C "$TARGET_DIR/$triple/release" mem index
    built+=("$name")
  done
done

if [ "$DRY_RUN" = 1 ]; then
  echo "abin-release: would publish $SHA:" >&2
  ls -la "$OUT" >&2
  exit 0
fi

for name in "${built[@]}"; do
  aws s3 cp \
    --cache-control "public, max-age=31536000, immutable" \
    "$OUT/$name" \
    "s3://$BUCKET/abin/$SHA/$name"
done

# Last, so the latest-pointer never names a half-uploaded release. `--no-latest-pointer`
# skips it, leaving the move to the one matrix job that runs after every other finished.
if [ "$LATEST_POINTER" = 1 ]; then
  move_latest_pointer
fi

echo "abin-release: published $SHA (${built[*]})" >&2
