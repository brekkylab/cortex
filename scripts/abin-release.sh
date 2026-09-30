#!/usr/bin/env bash
# Build virtx's own executables for the guest and publish them.
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
# The git sha of HEAD names the release. That is the whole of its identity — nothing
# downstream verifies the bytes — which is why a dirty tree is refused below.
#
# # The latest-pointer, and why moving it is separable
#
# `abin/latest` holds one line: the sha of the release a session should use when it has been
# told nothing. Everything below calls it the **latest-pointer**.
#
# It has to name a release that is *completely* uploaded. In one run that is just "write it
# last". Across a matrix of OSes building in parallel it cannot be: every job would move the
# latest-pointer itself, and whichever finished last would win regardless of whether the
# others had. So the builders run with `--no-latest-pointer`, and one job afterwards runs
# `--latest-pointer-only`.
#
# A release the latest-pointer never reaches is inert rather than broken: its tarballs sit
# under a sha nothing refers to. That is the property that makes the split safe.
set -euo pipefail

BUCKET="${ABIN_BUCKET:-cortex-dist-044443350235-us-east-1-an}"

die() { echo "abin-release: $*" >&2; exit 1; }

# Every OS this script knows how to build, in the order it builds them. One entry today: the
# guest is Linux and the guest is the only consumer. A host build — if the local console
# server is ever given an `/abin` — is a new line here and a new matrix entry in CI, and
# nothing else: the key layout already carries the OS.
ALL_OSES=(linux)

# `<arch>:<rust target>` per OS. `<arch>` is what goes in the filename and matches
# `std::env::consts::ARCH`, which is what the client builds its URL from.
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
# Validated before anything is built, so a typo is a sentence rather than a surprise after
# the first architecture has already been compiled and uploaded.
for os in "${OSES[@]}"; do targets_for "$os" >/dev/null; done

# Credentials come from wherever the aws CLI normally finds them. On a laptop that is a
# named profile and there is one obvious choice; in CI it is the environment, and naming a
# profile that does not exist there is an error rather than a default. So this fills in the
# laptop case and otherwise keeps out of the way — `--profile` is never passed explicitly,
# because doing so would override the environment it is trying to defer to.
if [ -z "${AWS_PROFILE:-}" ] && [ -z "${AWS_ACCESS_KEY_ID:-}" ]; then
  export AWS_PROFILE=brekkylab
fi

# Prerequisites, said as sentences rather than left to fail as tool errors — and only the
# ones this invocation will actually reach. Moving the latest-pointer compiles nothing.
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

# The sha is the identity. A tarball built from uncommitted changes and filed under this
# sha would make the path lie, and nothing downstream would catch it.
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

# Where cargo actually writes. Hardcoding `target` is wrong wherever CARGO_TARGET_DIR is
# set, which is most CI and any shared-cache setup — and it would fail at the `tar` below
# with a missing path rather than anywhere informative.
TARGET_DIR="${CARGO_TARGET_DIR:-target}"

built=()
for os in "${OSES[@]}"; do
  for pair in $(targets_for "$os"); do
    arch="${pair%%:*}"
    triple="${pair##*:}"
    name="abin-$os-$arch.tar.gz"
    echo "abin-release: building $triple" >&2
    cargo zigbuild -p virtx-exec-mem -p virtx-exec-index --release --target "$triple"
    # Flat: `mem` and `index` at the top level, which is exactly what /abin holds.
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

# Last, so there is no window in which the latest-pointer names a half-uploaded release.
# Suppressed by `--no-latest-pointer`, which is how a matrix leaves the move to the one job
# that knows every other has finished.
if [ "$LATEST_POINTER" = 1 ]; then
  move_latest_pointer
fi

echo "abin-release: published $SHA (${built[*]})" >&2
