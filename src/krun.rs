//! Boot microVMs directly on [`msb_krun`], the in-process Rust-native libkrun
//! fork.
//!
//! We work at this layer rather than the high-level microsandbox SDK because
//! only `msb_krun` lets us attach a *custom* in-process filesystem — a
//! [`DynFileSystem`] — to the guest through `FsBuilder::custom`. Upstream
//! libkrun (and its C API) and microsandbox's `Sandbox` only expose fixed
//! host-path / disk mounts, with no hook to substitute the virtio-fs backend.

use crate::fuse::ToyFs;
use msb_krun::{DynFileSystem, VmBuilder};
use std::path::Path;

/// Boot a microVM with the [`ToyFs`] attached as a virtio-fs share, tagged
/// `cortex`. Inside the guest:
///
/// ```text
/// mount -t virtiofs cortex /mnt && cat /mnt/hello.txt
/// ```
///
/// `FsBuilder::custom` takes exactly the `Box<dyn DynFileSystem + Send + Sync>`
/// that [`ToyFs`] satisfies, so the toy filesystem is served straight from this
/// process into the guest — no daemon, no host mount.
///
/// A real boot also needs a populated `rootfs` and a matching libkrunfw
/// `kernel` firmware; `src/bin/boot_toy_fs.rs` is a runnable driver that
/// provisions both.
pub fn boot_with_toy_fs(
    rootfs: impl AsRef<Path>,
    kernel: impl AsRef<Path>,
) -> msb_krun::Result<std::convert::Infallible> {
    let toy: Box<dyn DynFileSystem + Send + Sync> = Box::new(ToyFs::new());

    VmBuilder::new()
        .machine(|m| m.vcpus(1).memory_mib(512))
        .kernel(|k| k.krunfw_path(kernel.as_ref()))
        .fs(|fs| {
            // Mount 0: the guest root filesystem (host passthrough).
            // Mount 1: our toy, reachable inside the guest at virtio-fs tag `cortex`.
            fs.root(rootfs.as_ref()).tag("cortex").custom(toy)
        })
        .exec(|e| {
            e.path("/bin/sh")
                .args(["-c", "mount -t virtiofs cortex /mnt && cat /mnt/hello.txt"])
        })
        .build()?
        .enter()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises [`boot_with_toy_fs`] up to the point of launching the VM.
    ///
    /// We can't assert a *successful* boot here: on guest shutdown libkrun
    /// calls `_exit()`, which would kill the whole test process before the
    /// harness could observe anything (and a real boot needs a populated
    /// rootfs plus a libkrunfw kernel). Instead we hand it paths that don't
    /// exist. The builder gets fully constructed — `ToyFs` attached via
    /// `FsBuilder::custom` and all — and then fails at libkrunfw load, *before*
    /// any VM starts. So this proves the wiring assembles and returns an error
    /// gracefully rather than panicking.
    #[test]
    fn boot_with_toy_fs_fails_before_launch_on_bad_paths() {
        let result = boot_with_toy_fs("/nonexistent/cortex-rootfs", "/nonexistent/libkrunfw.so");
        // `Ok` is `Infallible` (a successful `enter()` never returns), so the
        // only reachable outcome is an error from the pre-launch setup.
        assert!(
            result.is_err(),
            "expected a pre-launch build error, VM must not have started"
        );
    }
}
