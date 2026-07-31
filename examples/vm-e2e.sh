#!/usr/bin/env bash
# VM end-to-end: run a predefined Executable from INSIDE a microsandbox VM
# against the host Workspace, via the guest `wsx` CLI -> host `exec-server`.
#
# Proves the forwarding channel with stock microsandbox (no msb modifications).
#
# Requires:
#   - msb (microsandbox) at $MSB or ~/.microsandbox/bin/msb, `msb doctor` green
#   - a cached `alpine` image (`msb pull alpine`)
#   - rustup target for the guest arch: aarch64/x86_64-unknown-linux-musl
#     (linked with the bundled LLVM lld via .cargo/config.toml's
#      `-C linker-flavor=ld.lld`, matching agent-k — no external cross gcc)
#
# Key findings baked in:
#   - egress to the host is its own policy group: boot with `--net-rule allow@host`
#     (the default `allow@public` does NOT include the host).
#   - the host is reachable from the guest at `host.microsandbox.internal`.
#   - pass wsx's env by `export`-ing inside the guest shell; `msb exec -e` did not
#     reliably propagate our vars, so wsx fell back to its default host:port.
set -euo pipefail

MSB="${MSB:-$HOME/.microsandbox/bin/msb}"
PORT="${PORT:-8137}"
TOKEN="${TOKEN:-secret}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG="$(mktemp -t exec-server.XXXXXX)"

case "$(uname -m)" in
  arm64 | aarch64) TGT="aarch64-unknown-linux-musl" ;;
  *) TGT="x86_64-unknown-linux-musl" ;;
esac

SRV="" ; SB=""
cleanup() {
  [ -n "$SRV" ] && kill "$SRV" 2>/dev/null || true
  [ -n "$SB" ] && "$MSB" rm --force "$SB" >/dev/null 2>&1 || true
  rm -f "$LOG"
}
trap cleanup EXIT

fail() { printf '\n\342\235\214 FAIL: %s\n' "$*" >&2; [ -s "$LOG" ] && { echo "--- exec-server log ---"; cat "$LOG"; }; exit 1; }

printf '\n\342\226\266 cortex \302\267 VM\342\206\222host Executable forwarding (end-to-end)\n\n'

printf '[1/4] Build\n'
cargo build -q --manifest-path "$ROOT/Cargo.toml" -p exec-server || fail "build exec-server"
printf '      \342\234\223 host exec-server\n'
( cd "$ROOT/wsx" && cargo build -q --release --target "$TGT" ) || fail "build wsx"
WSX="$ROOT/target/$TGT/release/wsx"
printf '      \342\234\223 guest wsx (%s, static ELF)\n' "$TGT"

printf '[2/4] Host exec-server\n'
WSX_LISTEN="0.0.0.0:$PORT" WSX_TOKEN="$TOKEN" "$ROOT/target/debug/exec-server" >"$LOG" 2>&1 &
SRV=$!
disown
sleep 1
kill -0 "$SRV" 2>/dev/null || fail "exec-server did not start"
printf '      \342\234\223 listening on 0.0.0.0:%s (token required)\n' "$PORT"

printf '[3/4] microsandbox VM\n'
SB="$("$MSB" run -d alpine --net-rule "allow@host" --copy-file "$WSX:/usr/local/bin/wsx" | tail -1)"
printf '      \342\234\223 booted alpine sandbox %s (egress policy: allow@host)\n' "$SB"
printf '      \342\234\223 planted wsx at /usr/local/bin/wsx\n'

# Run one command line inside the guest, exporting wsx's env in the guest shell.
gexec() {
  "$MSB" exec "$SB" -- sh -c \
    "export WSX_HOST=host.microsandbox.internal:$PORT WSX_TOKEN=$TOKEN; $1"
}

printf '[4/4] Run predefined Executables from INSIDE the VM (guest \342\206\222 host)\n'

gexec '/usr/local/bin/wsx write vm.txt "hi from VM"' || fail "guest wsx write"
printf '      $ wsx write vm.txt "hi from VM"\n        \342\234\223 written to the host workspace\n'

got="$(gexec '/usr/local/bin/wsx cat vm.txt')" || fail "guest wsx cat"
printf '      $ wsx cat vm.txt\n        \342\206\222 "%s"' "$got"
[ "$got" = "hi from VM" ] || { printf '\n'; fail "cat returned \"$got\", expected \"hi from VM\""; }
printf '   \342\234\223 round-trip verified\n'

listing="$(gexec '/usr/local/bin/wsx ls .' | tr '\n' ' ')"
printf '      $ wsx ls .\n        \342\206\222 %s\n' "$listing"

printf '\n\342\234\205 VM e2e PASSED\n'
printf '   The guest ran host-side executables against the shared workspace over wsx \342\206\222 exec-server.\n'
printf '   Cleaning up: host exec-server stopped, sandbox %s removed.\n' "$SB"
