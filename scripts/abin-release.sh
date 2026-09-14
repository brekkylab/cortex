#!/usr/bin/env bash
# Build cortex's own executables for the guest and publish them.
#
#   scripts/abin-release.sh [--dry-run]
#
# The git sha of HEAD names the release. That is the whole of its identity — nothing
# downstream verifies the bytes — which is why a dirty tree is refused below.
set -euo pipefail

BUCKET="${ABIN_BUCKET:-cortex-dist-044443350235-us-east-1-an}"
PROFILE="${AWS_PROFILE:-brekkylab}"
DRY_RUN=0
[ "${1:-}" = "--dry-run" ] && DRY_RUN=1

die() { echo "abin-release: $*" >&2; exit 1; }

# Prerequisites, said as sentences rather than left to fail as tool errors.
command -v zig >/dev/null || die "zig is not on PATH (brew install zig)"
command -v cargo-zigbuild >/dev/null || die "cargo-zigbuild is not installed (cargo install cargo-zigbuild)"
command -v aws >/dev/null || die "the aws CLI is not on PATH"
for t in aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
  rustup target list --installed | grep -qx "$t" || die "missing target $t (rustup target add $t)"
done

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

for pair in "aarch64:aarch64-unknown-linux-musl" "x86_64:x86_64-unknown-linux-musl"; do
  arch="${pair%%:*}"
  triple="${pair##*:}"
  echo "abin-release: building $triple" >&2
  cargo zigbuild -p cortex-exec-mem -p cortex-exec-index --release --target "$triple"
  # Flat: `mem` and `index` at the top level, which is exactly what /abin holds.
  tar czf "$OUT/abin-linux-$arch.tar.gz" -C "target/$triple/release" mem index
done

if [ "$DRY_RUN" = 1 ]; then
  echo "abin-release: would publish $SHA:" >&2
  ls -la "$OUT" >&2
  exit 0
fi

for arch in aarch64 x86_64; do
  aws s3 cp --profile "$PROFILE" \
    --cache-control "public, max-age=31536000, immutable" \
    "$OUT/abin-linux-$arch.tar.gz" \
    "s3://$BUCKET/abin/$SHA/abin-linux-$arch.tar.gz"
done

# Last, so there is no window in which the pointer names a half-uploaded release.
printf '%s\n' "$SHA" > "$OUT/latest"
aws s3 cp --profile "$PROFILE" --cache-control "no-cache" \
  "$OUT/latest" "s3://$BUCKET/abin/latest"

echo "abin-release: published $SHA" >&2
