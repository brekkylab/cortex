// One console: boot alpine with `jq` installed, run one command in it, and end.
//
//     node examples/hello.mjs
//
// Needs `cortex-krun` in the cortex cache `bin` (`$CORTEX_HOME/bin` if set); `ensureCortex()` fetches it.

import { createRequire } from 'node:module'

const { ConsoleClient, Recipe } = createRequire(import.meta.url)('../index.js')

const console_ = await ConsoleClient.builder()
  .image(new Recipe('alpine:latest').step('apk add --no-cache jq'))
  .build()

try {
  const result = await console_.exec(['sh', '-c', `echo '{"hello": "cortex"}' | jq -r .hello`])
  process.stdout.write(result.stdout)
  process.stderr.write(result.stderr)
  console.log(`exit code: ${result.code}`)
} finally {
  // Closing says `quit`, and the server tears the session down.
  await console_.close()
}
