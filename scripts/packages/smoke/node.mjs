// The Node package as a user installs it, on the platform this runs on.
//
// Run from a project that installed `@brekkylab/cortex`, so it resolves the way that
// project's code would. What this platform can do is said in the environment:
//   SMOKE_MOUNT   1 if a FUSE provider is installed here, else 0
//   SMOKE_SERVER  1 if a cortex-krun release is published for this platform, else 0
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

const cortex = require('@brekkylab/cortex')
check(typeof cortex.ConsoleClient === 'function', 'the package loads')

// Only this platform's binary, as npm's os/cpu/libc filter chose it.
const scope = path.join(process.cwd(), 'node_modules', '@brekkylab')
const installed = fs.readdirSync(scope).sort()
console.log(`  installed: ${installed.join(' ')}`)
check(installed.length === 2, 'exactly one platform package was installed beside the root')

let fuse = true
try {
  cortex.mountSupport()
} catch (e) {
  fuse = false
  console.log(`  mountSupport: ${e.message}`)
}
check(fuse === want('SMOKE_MOUNT'), `mountSupport says ${fuse ? 'yes' : 'no'}`)

if (fuse) {
  const point = fs.mkdtempSync(path.join(os.tmpdir(), 'cortex-smoke-'))
  const m = new cortex.HostMount(new cortex.Directory().withFile('a.txt', 'hi'), point)
  check(fs.readFileSync(path.join(point, 'a.txt'), 'utf8') === 'hi', 'HostMount serves its tree')
  await m.unmount()
  check(!fs.existsSync(path.join(point, 'a.txt')), 'HostMount.unmount() takes it down')

  if (process.platform !== 'win32') {
    const child = fs.mkdtempSync(path.join(os.tmpdir(), 'cortex-smoke-killed-'))
    const script = `const c=require(${JSON.stringify(require.resolve('@brekkylab/cortex'))});` +
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

let server = null
try {
  server = await cortex.ensureCortex()
  console.log(`  ensureCortex: ${server}`)
} catch (e) {
  console.log(`  ensureCortex: ${e.message}`)
  check(!want('SMOKE_SERVER') && e.message.includes('no cortex-krun release is published'), 'ensureCortex says no release is published here')
}
if (server) {
  check(want('SMOKE_SERVER'), 'ensureCortex fetched the server')
  const exe = process.platform === 'win32' ? '.exe' : ''
  check(fs.existsSync(path.join(server, `cortex-krun${exe}`)), `cortex-krun${exe} is in ${server}`)
  // The server runs on this machine, and answers -- no VM needed to ask its version.
  const images = await cortex.ImageClient.tryNew()
  const version = await images.version()
  await images.close()
  check(typeof version === 'string' && version.length > 0, `the server answers (protocol ${version})`)
  // And the VM process it would spawn starts, and says what it can make here.
  let caps = null
  try {
    caps = execFileSync(path.join(server, `cortex-krun-host${exe}`), ['--capabilities'], { encoding: 'utf8' })
    console.log(`  cortex-krun-host --capabilities: [${caps.trim().split(/\s+/).filter(Boolean).join(' ')}]`)
  } catch (e) {
    console.log(`  cortex-krun-host --capabilities: ${e.message}`)
  }
  check(caps !== null, 'cortex-krun-host starts')
}

if (server && want('SMOKE_VM')) {
  const host = fs.mkdtempSync(path.join(os.tmpdir(), 'cortex-smoke-host-'))
  fs.writeFileSync(path.join(host, 'from-host.txt'), 'by path')
  let b = cortex.ConsoleClient.builder().image(new cortex.Recipe('alpine:latest')).mount(host, '/host')
  const point = fs.mkdtempSync(path.join(os.tmpdir(), 'cortex-smoke-vm-'))
  const m = fuse ? new cortex.HostMount(new cortex.Directory().withFile('a.txt', 'a Directory'), point) : null
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
