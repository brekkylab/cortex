#!/usr/bin/env bash
# Build, codesign, and run the toy-FUSE microVM demo.
#
# On macOS, creating a VM via Hypervisor.framework requires the binary to carry
# the `com.apple.security.hypervisor` entitlement — but `cargo build` produces an
# unsigned binary, so it must be ad-hoc signed after every build or `VmCreate`
# fails. This wraps build → sign → run so `cargo run` gotchas don't bite.
#
# Usage: scripts/run-toy-fs.sh [extra cargo args...]
set -euo pipefail

cd "$(dirname "$0")/.."

PROFILE_DIR="debug"
BIN="target/${PROFILE_DIR}/apply_krun"

echo "==> building apply_krun"
cargo build -p cortex-fs --features krun --bin apply_krun "$@"

echo "==> codesigning with hypervisor entitlement"
codesign --entitlements macos-entitlements.plist -s - --force "$BIN"

echo "==> running"
exec "$BIN"
