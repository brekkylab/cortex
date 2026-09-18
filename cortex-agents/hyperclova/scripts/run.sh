#!/usr/bin/env bash
# Build once, then run with the credentials the environment already has.
#   CLOVASTUDIO_API_KEY   required
#   AWS_*                 only with --s3; `eval "$(aws configure export-credentials --format env)"`
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
if [ -f "$here/.env" ]; then set -a; . "$here/.env"; set +a; fi
cargo build -q --manifest-path "$here/../../Cargo.toml" -p cortex-agent-hyperclova -p cortex-exec-mem
exec "$here/../../target/debug/cortex-hyperclova" "$@"
