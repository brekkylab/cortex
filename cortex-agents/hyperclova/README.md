# cortex-hyperclova

HyperCLOVA X working over a Cortex tree, inside one actor's read permissions.

```
cortex-hyperclova --actor 구매팀 "협력사 신용등급 변동표·납기 이력·여신한도를 대조해 이번 주 위험 거래처와 대체 후보를 뽑아 주세요"
cortex-hyperclova --actor 인사팀 --tree-only
cortex-hyperclova --actor 재무팀 --model HCX-007 --s3 my-bucket/finance
```

## What runs

- **The tree** is a `WorkFs`: every top-level directory of `--workspace` is a mount. Local folders are served by `PassthroughFs`; with `--s3 bucket[/prefix]` one of them (`--s3-at`, default `재무팀`) comes from an S3-compatible bucket through `S3Fs` instead. Ncloud Object Storage speaks the S3 API, so it mounts the same way with `--s3-endpoint https://kr.object.ncloudstorage.com`. Nothing is copied or indexed ahead of time; the original stays where it is.
- **The actor** is a department. `AclFs` wraps the tree and answers for that actor: a `read_at` on a path outside the actor's readership fails with `PermissionDenied` before any bytes move. Names stay listable. The rules are `정책/acl.json`, a longest-prefix match over root-relative paths.
- **The model** is `HCX-005` or `HCX-007`, reached through CLOVA Studio's OpenAI-compatible endpoint. ailoy's `ChatCompletion` schema is that wire, so the endpoint is one provider entry and the agent loop is ailoy's own. `HCX-007` accepts Function Calling only with `reasoning_effort: "none"`, which is sent for that model by default (`--reasoning-effort` overrides).
- **The tools** are `ls`, `read`, `search`, `write_report`, and — when `mem` is built — `remember` and `recall`. All of them go through the actor's `AclFs`, so a denial reaches the model as a tool result it has to report rather than data it should not have seen.
- **The output** goes under `산출물/<actor>/`. `write_report` accepts a citation only for a file that was actually opened in this run, and the report inherits the narrowest readership among its citations; a sidecar `<report>.acl.json` records readers, sources, author and model, and `AclFs` enforces it — a prefix the policy opens to everyone, a sidecar can close again. Every tool call is appended to an audit log written beside the report as `감사로그-<time>.jsonl`, readable by the actor alone; after the run, every source the tree refused is checked against the report's text, so the summary of what could not be read comes from the log and not from the model.
- **Memory** (`remember` / `recall`, over cortex's `mem`) is one store per actor under the same folder, so a conclusion drawn from files one department may read is recalled by that department only.

## Running

```
export CLOVASTUDIO_API_KEY=…                                   # CLOVA Studio test or service key
cargo build -p cortex-agent-hyperclova -p cortex-exec-mem      # mem enables remember/recall
./target/debug/cortex-hyperclova --actor 구매팀
```

`scripts/run.sh` does the build and reads a `.env` beside it if there is one. For `--s3`, put AWS-style credentials in the environment first (`eval "$(aws configure export-credentials --format env)"`).

`CLOVASTUDIO_OPENAI_URL` overrides the endpoint (default `https://clovastudio.stream.ntruss.com/v1/openai/chat/completions`).

## The example workspace

`examples/procurement/` is a small manufacturer's shared drive: purchasing (규정·발주·협력사평가), finance (여신한도·실적), HR (직무기술서), minutes and the access policy. The policy makes the three departments see three different trees:

| path | 구매팀 | 재무팀 | 인사팀 |
|---|---|---|---|
| `구매팀/협력사평가/*` (신용 정보) | read | read | 🔒 |
| `구매팀/발주/*` (납기 이력) | read | 🔒 | 🔒 |
| `재무팀/*` (여신 한도·실적) | 🔒 | read | 🔒 |
| `인사팀/*` | 🔒 | 🔒 | read |
| `회의록/*`, `정책/*`, `구매팀/구매규정-v7.md` | read | read | read |

The same question — cross-check credit-rating changes, delivery history and credit limits for this week's at-risk suppliers — therefore ends three ways: purchasing gets a report it alone may read (delivery history is purchasing-only), finance gets one with the credit-limit column filled and the delivery history marked out of reach, and HR is told the report cannot be written and who holds the data.

All names, figures and companies in the example are fictional.
