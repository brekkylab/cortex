// The Node package as a user installs it, on the platform this runs on.
//
// Run from a project that installed `virtx`, so it resolves the way that
// project's code would. What this platform can do is said in the environment:
//   SMOKE_MOUNT   1 if a FUSE provider is installed here, else 0
//   SMOKE_SERVER  1 if a virtx-uvm release is published for this platform, else 0 --
//                 fetched by the first client built on it, into $VIRTX_HOME
//   SMOKE_VM      1 if this machine can boot one (KVM or HVF), else 0
import { execFileSync, spawn } from 'node:child_process'
import fs from 'node:fs'
import { createRequire } from 'node:module'
import os from 'node:os'
import path from 'node:path'

const require = createRequire(path.join(process.cwd(), 'noop.js'))
const want = (name) => process.env[name] === '1'
const failures = []
const check = (ok, what) => {
  console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`)
  if (!ok) failures.push(what)
}
const until = async (cond, ms) => {
  const end = Date.now() + ms
  while (Date.now() < end) {
    if (cond()) return true
    await new Promise((r) => setTimeout(r, 50))
  }
  return cond()
}

const virtx = require('@brekkylab/virtx')
check(typeof virtx.ConsoleClient === 'function', 'the package loads')

// Only this platform's binary, as npm's os/cpu/libc filter chose it.
const installed = fs.readdirSync(path.join(process.cwd(), 'node_modules', '@brekkylab')).sort()
console.log(`  installed: ${installed.join(' ')}`)
check(installed.length === 2, 'exactly one platform package was installed beside the root')

let fuse = true
try {
  virtx.mountSupport()
} catch (e) {
  fuse = false
  console.log(`  mountSupport: ${e.message}`)
}
check(fuse === want('SMOKE_MOUNT'), `mountSupport says ${fuse ? 'yes' : 'no'}`)

if (fuse) {
  const point = fs.mkdtempSync(path.join(os.tmpdir(), 'virtx-smoke-'))
  const m = new virtx.HostMount(new virtx.Directory().withFile('a.txt', 'hi'), point)
  check(fs.readFileSync(path.join(point, 'a.txt'), 'utf8') === 'hi', 'HostMount serves its tree')
  await m.unmount()
  check(!fs.existsSync(path.join(point, 'a.txt')), 'HostMount.unmount() takes it down')

  if (process.platform !== 'win32') {
    const child = fs.mkdtempSync(path.join(os.tmpdir(), 'virtx-smoke-killed-'))
    const script = `const c=require(${JSON.stringify(require.resolve('@brekkylab/virtx'))});` +
      `globalThis.m=new c.HostMount(new c.Directory().withFile('a.txt','hi'),${JSON.stringify(child)});` +
      `require('fs').writeFileSync(${JSON.stringify(child + '.ready')},'');setInterval(()=>{},1e9)`
    const p = spawn(process.execPath, ['-e', script], { stdio: 'ignore' })
    const ready = await until(() => fs.existsSync(child + '.ready'), 15000)
    check(ready && fs.existsSync(path.join(child, 'a.txt')), 'a child process mounted')
    p.kill('SIGKILL')
    await new Promise((r) => p.on('exit', r))
    check(await until(() => !fs.existsSync(path.join(child, 'a.txt')), 5000), 'the mount of a SIGKILLed process came down')
  }
}

// The server, fetched by the first client built on it: no `ensureVirtx()` first.
let images = null
try {
  images = await virtx.ImageClient.tryNew()
} catch (e) {
  console.log(`  ImageClient.tryNew: ${e.message}`)
  check(!want('SMOKE_SERVER') && e.message.includes('no virtx-uvm release is published'), 'building on the server says no release is published here')
}
let server = null
if (images) {
  check(want('SMOKE_SERVER'), 'building an ImageClient fetched the server')
  // The server runs on this machine, and answers -- no VM needed to ask its version.
  const version = await images.version()
  await images.close()
  check(typeof version === 'string' && version.length > 0, `the server answers (protocol ${version})`)
  // `ensureVirtx` finds it there, and says where.
  server = await virtx.ensureVirtx()
  console.log(`  ensureVirtx: ${server}`)
  const exe = process.platform === 'win32' ? '.exe' : ''
  check(fs.existsSync(path.join(server, `virtx-uvm${exe}`)), `virtx-uvm${exe} is in ${server}`)
  // And the VM process it would spawn starts, and says what it can make here.
  let caps = null
  try {
    caps = execFileSync(path.join(server, `virtx-uvm-host${exe}`), ['--capabilities'], { encoding: 'utf8' })
    console.log(`  virtx-uvm-host --capabilities: [${caps.trim().split(/\s+/).filter(Boolean).join(' ')}]`)
  } catch (e) {
    console.log(`  virtx-uvm-host --capabilities: ${e.message}`)
  }
  check(caps !== null, 'virtx-uvm-host starts')
}

if (server && want('SMOKE_VM')) {
  const host = fs.mkdtempSync(path.join(os.tmpdir(), 'virtx-smoke-host-'))
  fs.writeFileSync(path.join(host, 'from-host.txt'), 'by path')
  let b = virtx.ConsoleClient.builder().image(new virtx.Recipe('alpine:latest')).mount(host, '/host')
  const point = fs.mkdtempSync(path.join(os.tmpdir(), 'virtx-smoke-vm-'))
  const m = fuse ? new virtx.HostMount(new virtx.Directory().withFile('a.txt', 'a Directory'), point) : null
  if (m) b = b.mount(m, '/work')
  const c = await b.build()
  const r = await c.exec(['sh', '-c', 'uname -m; cat /host/from-host.txt; echo; [ -d /work ] && cat /work/a.txt; echo written > /host/from-vm.txt'], 120000)
  const out = r.stdout.toString()
  console.log(`  vm: ${out.trim().replace(/\n/g, ' | ')}`)
  check(r.code === 0 && out.includes('by path') && (!m || out.includes('a Directory')), 'a VM session reads its mounts')
  check(fs.readFileSync(path.join(host, 'from-vm.txt'), 'utf8').trim() === 'written', "the host sees the VM's write")
  await c.close()
  if (m) await m.unmount()
}

console.log(failures.length ? `FAILED: ${failures.join('; ')}` : 'ALL PASS')
process.exit(failures.length ? 1 : 0)
