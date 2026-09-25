import assert from 'node:assert/strict'
import { mkdtempSync, mkdirSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { createRequire } from 'node:module'

const cortex = createRequire(import.meta.url)('../index.js')
const { Console, Directory, Image, NetworkAccess, Step } = cortex

const tempDir = () => mkdtempSync(join(tmpdir(), 'cortex-'))

test('an Image is a value', () => {
  const base = new Image().base('python:3.12-slim')
  const extended = base.step('pip install duckdb').step(Step.env('TZ', 'UTC'))

  assert.match(extended.toString(), /python:3\.12-slim/)
  assert.match(extended.toString(), /duckdb/)
  assert.doesNotMatch(base.toString(), /duckdb/)
})

test('an Image from a Dockerfile', () => {
  const image = Image.fromDockerfile('FROM alpine:3.20\nRUN apk add jq\n')
  assert.match(image.toString(), /alpine:3\.20/)
})

test('NetworkAccess', () => {
  const network = NetworkAccess.host().withHostPorts([8080])
  assert.equal(network.reach, 'host')
  assert.deepEqual(network.hostPorts, [8080])
  assert.deepEqual(NetworkAccess.none().hostPorts, [])
})

test('a Directory refuses a file under a mount', () => {
  const directory = new Directory().withMount('project', tempDir())
  assert.throws(() => directory.addFile('project/notes.md', 'under a mount'), { code: 'InvalidInput' })
})

test('a HostMount serves the Directory', { skip: !cortex.HostMount && 'built without `mount`' }, () => {
  const mountpoint = join(tempDir(), 'mnt')
  mkdirSync(mountpoint)
  const directory = new Directory().withFile('notes/today.md', Buffer.from('ship the release'))

  const mount = new cortex.HostMount(directory, mountpoint)
  assert.equal(mount.mountpoint, mountpoint)
  assert.equal(readFileSync(join(mountpoint, 'notes', 'today.md'), 'utf8'), 'ship the release')
  // The mount owns the tree now.
  assert.throws(() => directory.addFile('more.md', ''), { code: 'INVALID_ARG' })
})

test('building without a server fails and spends the builder', async () => {
  const builder = Console.builder()
  await assert.rejects(builder.build(), { code: 'CORTEX_ERROR' })
  assert.throws(() => builder.vcpus(2), { code: 'INVALID_ARG' })
})

test('building against a missing binary fails', async () => {
  await assert.rejects(
    Console.builder().stdioClient(['cortex-no-such-console-server']).build(),
    { code: 'CORTEX_ERROR' },
  )
})

// Against a real console server, named by `$CORTEX_CONSOLE` (`cortex-krun`, say).
const SERVER = process.env.CORTEX_CONSOLE

test('exec, read and write', { skip: !SERVER && 'set $CORTEX_CONSOLE' }, async () => {
  const console_ = await Console.builder()
    .stdioClient([SERVER])
    .image(new Image('python:3.12-slim-trixie'))
    .mount(tempDir(), '/work')
    .network(NetworkAccess.none())
    .build()
  try {
    assert.deepEqual(console_.mounts, ['/work'])

    assert.equal(await console_.write('/work/hello.txt', 'hi'), 2)
    const result = await console_.exec(['cat', '/work/hello.txt'])
    assert.equal(result.code, 0)
    assert.equal(result.stdout.toString(), 'hi')

    const read = await console_.read('/work/hello.txt')
    assert.equal(read.data.toString(), 'hi')
    assert.equal(read.size, 2)
  } finally {
    await console_.close()
  }
})
