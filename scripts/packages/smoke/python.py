"""The Python package as a user installs it, on the platform this runs on.

What this platform can do is said in the environment, as for ``node.mjs``:
SMOKE_MOUNT, SMOKE_SERVER, SMOKE_VM.
"""
import asyncio, os, signal, subprocess, sys, tempfile, time

import virtx

want = lambda name: os.environ.get(name) == "1"
failures = []


def check(ok, what):
    print(("PASS " if ok else "FAIL ") + what, flush=True)
    if not ok:
        failures.append(what)


def until(cond, secs):
    end = time.time() + secs
    while time.time() < end:
        if cond():
            return True
        time.sleep(0.05)
    return cond()


async def main():
    check(hasattr(virtx, "ConsoleClient"), f"the package loads from {os.path.dirname(virtx.__file__)}")

    try:
        virtx.mount_support()
        fuse = True
    except OSError as e:
        fuse = False
        print(f"  mount_support: {e}")
    check(fuse == want("SMOKE_MOUNT"), f"mount_support says {'yes' if fuse else 'no'}")

    if fuse:
        point = tempfile.mkdtemp(prefix="virtx-smoke-")
        m = virtx.HostMount(virtx.Directory().with_file("a.txt", "hi"), point)
        check(open(os.path.join(point, "a.txt")).read() == "hi", "HostMount serves its tree")
        del m
        check(not os.path.exists(os.path.join(point, "a.txt")), "dropping the HostMount takes it down")

        if sys.platform != "win32":
            child = tempfile.mkdtemp(prefix="virtx-smoke-killed-")
            code = (
                "import virtx, time\n"
                f"m = virtx.HostMount(virtx.Directory().with_file('a.txt', 'hi'), {child!r})\n"
                f"open({child + '.ready'!r}, 'w').close()\n"
                "time.sleep(1e6)\n"
            )
            p = subprocess.Popen([sys.executable, "-c", code])
            ready = until(lambda: os.path.exists(child + ".ready"), 15)
            check(ready and os.path.exists(os.path.join(child, "a.txt")), "a child process mounted")
            p.send_signal(signal.SIGKILL)
            p.wait()
            check(until(lambda: not os.path.exists(os.path.join(child, "a.txt")), 5), "the mount of a SIGKILLed process came down")

    # The server, fetched by the first client built on it: no `ensure_virtx()` first.
    images = None
    try:
        images = await virtx.ImageClient.try_new()
    except Exception as e:
        print(f"  ImageClient.try_new: {e}")
        check(not want("SMOKE_SERVER") and "no virtx-uvm release is published" in str(e), "building on the server says no release is published here")
    server = None
    if images:
        check(want("SMOKE_SERVER"), "building an ImageClient fetched the server")
        # The server runs on this machine, and answers -- no VM needed to ask its version.
        version = await images.version()
        await images.close()
        check(bool(version), f"the server answers (protocol {version})")
        # `ensure_virtx` finds it there, and says where.
        server = await virtx.ensure_virtx()
        print(f"  ensure_virtx: {server}")
        exe = ".exe" if sys.platform == "win32" else ""
        check(os.path.isfile(os.path.join(server, f"virtx-uvm{exe}")), f"virtx-uvm{exe} is in {server}")
        # And the VM process it would spawn starts, and says what it can make here.
        caps = subprocess.run([os.path.join(server, f"virtx-uvm-host{exe}"), "--capabilities"], capture_output=True, text=True)
        print(f"  virtx-uvm-host --capabilities: [{' '.join(caps.stdout.split())}]")
        check(caps.returncode == 0, "virtx-uvm-host starts")

    if server and want("SMOKE_VM"):
        host = tempfile.mkdtemp(prefix="virtx-smoke-host-")
        open(os.path.join(host, "from-host.txt"), "w").write("by path")
        b = virtx.ConsoleClient.builder().image(virtx.Recipe("alpine:latest")).mount(host, "/host")
        m = None
        if fuse:
            m = virtx.HostMount(virtx.Directory().with_file("a.txt", "a Directory"), tempfile.mkdtemp(prefix="virtx-smoke-vm-"))
            b = b.mount(m, "/work")
        async with await b.build() as c:
            r = await c.exec(["sh", "-c", "uname -m; cat /host/from-host.txt; echo; [ -d /work ] && cat /work/a.txt; echo written > /host/from-vm.txt"], 120_000)
            out = r.stdout.decode()
            print("  vm: " + out.strip().replace("\n", " | "))
            check(r.code == 0 and "by path" in out and (m is None or "a Directory" in out), "a VM session reads its mounts")
            check(open(os.path.join(host, "from-vm.txt")).read().strip() == "written", "the host sees the VM's write")
        del m

    print("FAILED: " + "; ".join(failures) if failures else "ALL PASS", flush=True)
    sys.exit(1 if failures else 0)


asyncio.run(main())
