#!/bin/sh
# Check `fuse_t.h` against FUSE-T's own headers: the layouts `abi_check.c` prints, built
# against each, and the signatures of every call and operation the shim uses. Needs FUSE-T
# installed (`brew install --cask fuse-t`) and runs on macOS; exits nonzero on any difference.
#
#   contrib/fuse_t/check-abi.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
cflags=$(pkg-config --cflags fuse-t) || {
    echo "check-abi: FUSE-T's headers are not installed: brew install --cask fuse-t" >&2
    exit 1
}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The flag `build.rs` builds the shim with, and FUSE-T's headers insist on.
# shellcheck disable=SC2086
cc -Wall -Werror -D_FILE_OFFSET_BITS=64 -DCORTEX_ABI_REAL $cflags -o "$work/real" "$here/abi_check.c"
cc -Wall -Werror -D_FILE_OFFSET_BITS=64 -I"$here" -o "$work/ours" "$here/abi_check.c"
"$work/real" > "$work/real.txt"
"$work/ours" > "$work/ours.txt"
if ! diff -u "$work/real.txt" "$work/ours.txt"; then
    echo "check-abi: fuse_t.h lays out a type differently from FUSE-T's headers (above)" >&2
    exit 1
fi
echo "check-abi: $(wc -l < "$work/ours.txt" | tr -d ' ') layouts agree"

# The signatures, as the preprocessor leaves them: each call the shim resolves with `dlsym`,
# and each operation it sets, spaced alike and compared as text.
printf '#define FUSE_USE_VERSION 26\n#include <fuse_lowlevel.h>\n' > "$work/real.c"
printf '#include "fuse_t.h"\n' > "$work/ours.c"
# shellcheck disable=SC2086
cc -E -P -D_FILE_OFFSET_BITS=64 $cflags "$work/real.c" > "$work/real.i"
cc -E -P -D_FILE_OFFSET_BITS=64 -I"$here" "$work/ours.c" > "$work/ours.i"
python3 - "$work/real.i" "$work/ours.i" "$here/shim.c" <<'EOF'
import re, sys

def text(path):
    return re.sub(r"\s+", " ", open(path).read())

def ops(src):
    body = re.search(r"struct fuse_lowlevel_ops \{(.*?)\};", src).group(1)
    # A member is `ret (*name)(...)`, or `cortex_fuse_t_unused_op name` for one the shim
    # never sets.
    def name(decl):
        pointer = re.search(r"\(\s*\*\s*(\w+)\s*\)", decl)
        return pointer.group(1) if pointer else decl.split()[-1]
    return {name(d): d.strip() for d in body.split(";") if d.strip()}

def normal(decl):
    return re.sub(r"\s*([(),*])\s*", r"\1", decl).strip()

real, ours, shim = (text(p) for p in sys.argv[1:4])
# `fuse_version` besides: the one call made before the shim knows what it has loaded.
calls = re.findall(r"X\((fuse_\w+)\)", shim) + ["fuse_version"]
used = re.findall(r"\.(\w+) = ll_\w+", shim)
bad = []
for name in calls:
    pattern = r"[\w\s\*]*\b" + name + r"\s*\([^;]*\)\s*;"
    theirs, mine = re.search(pattern, real), re.search(pattern, ours)
    if not mine or normal(theirs.group(0)) != normal(mine.group(0)):
        bad.append(f"{name}:\n  FUSE-T:   {theirs.group(0).strip()}\n  fuse_t.h: {mine.group(0).strip() if mine else 'missing'}")
real_ops, our_ops = ops(real), ops(ours)
for name in used:
    if normal(real_ops[name]) != normal(our_ops.get(name, "")):
        bad.append(f"op {name}:\n  FUSE-T:   {real_ops[name]}\n  fuse_t.h: {our_ops.get(name, 'missing')}")
if bad:
    print("check-abi: signatures differ from FUSE-T's headers:\n" + "\n".join(bad), file=sys.stderr)
    sys.exit(1)
print(f"check-abi: {len(calls)} calls and {len(used)} operations have FUSE-T's signatures")
EOF

# And the release: the shim trusts these declarations for FUSE-T's major version they were
# checked against, so one of another major version is for a person to look at before the
# shim may load it -- raise `CORTEX_FUSE_T_MAJOR` only once this has passed against it.
release=$(pkg-config --modversion fuse-t)
checked=$(sed -n 's/^#define CORTEX_FUSE_T_CHECKED "\(.*\)"$/\1/p' "$here/fuse_t.h")
major=$(sed -n 's/^#define CORTEX_FUSE_T_MAJOR \([0-9]*\)$/\1/p' "$here/fuse_t.h")
if [ "${release%%.*}" != "$major" ]; then
    echo "check-abi: FUSE-T $release is installed, and fuse_t.h trusts FUSE-T $major.x: check it, then raise CORTEX_FUSE_T_MAJOR" >&2
    exit 1
fi
if [ "$release" != "$checked" ]; then
    echo "check-abi: note: checked FUSE-T $release; fuse_t.h says $checked was the last, so set CORTEX_FUSE_T_CHECKED to $release"
fi
echo "check-abi: FUSE-T $release, within the $major.x fuse_t.h trusts"
