// The crate's quickstart, in JavaScript.
//
// A file held in memory beside a host directory, mounted on the host, and a console that
// runs a command against it.
//
//     CORTEX_CONSOLE=cortex-krun node examples/quickstart.mjs /path/to/project

import { mkdtempSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const { Console, Directory, HostMount, Image, NetworkAccess } = createRequire(import.meta.url)('../index.js')

const project = process.argv[2] ?? process.cwd()

// What the agent can see. A `Directory` is itself a filesystem, so a binding drives it like
// any single store.
const context = new Directory()
  .withFile('notes/today.md', 'ship the release')
  .withMount('project', project)

// Where the host can see it. It stays mounted for as long as something holds it — here, the
// variable and the console below.
const mount = new HostMount(context, mkdtempSync(join(tmpdir(), 'cortex-')))

// What the agent can do: a server that runs its commands, against that tree.
const console_ = await Console.builder()
  .stdioClient([process.env.CORTEX_CONSOLE ?? 'cortex-krun'])
  .image(new Image('python:3.12-slim-trixie'))
  .mount(mount, '/work')
  .network(NetworkAccess.none())
  .build()

try {
  const result = await console_.exec(['sh', '-c', 'wc -w /work/notes/today.md'])
  process.stdout.write(result.stdout)
} finally {
  await console_.close()
}
