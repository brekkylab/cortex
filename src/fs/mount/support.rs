//! Whether this host can mount at all, and what to install when it cannot.

use std::io;

/// Check this host has what its `mount` binding needs at run time, and if not, say what to
/// install.
///
/// A build with `mount` runs on a host without the provider: on macOS the shim `dlopen`s
/// libfuse-t on first use, and on Windows `dokan2.dll` loads on a mount's first call into it.
/// A missing provider costs a mount, not the process, and this finds it before a binding calls
/// in. On Windows that needs the binary linked with `/DELAYLOAD:dokan2.dll`, which
/// `rustc-link-arg` cannot request for a dependent (see the `mount` feature in `Cargo.toml`);
/// without it the DLL is needed to start at all.
///
/// Every binding's `try_new` calls this first; call it directly only to know ahead, e.g. to
/// hide a feature.
///
/// | Target | Checked | Otherwise |
/// |---|---|---|
/// | macOS | libfuse-t loaded, and the NFS server it starts is where it starts it from | `brew install --cask fuse-t` |
/// | Windows | `dokan2.dll` loads, and the `dokan2.sys` driver is installed | the Dokany 2 installer |
/// | Linux | `/dev/fuse`, and `fusermount3` or `fusermount` for a user that is not root | `fuse3` from the distribution |
///
/// `Ok` means the pieces are present, not that a mount will succeed (mount point, driver
/// state and user rights are still unchecked). Errors are [`io::ErrorKind::Unsupported`] with
/// the install instructions in the message.
pub fn mount_support() -> io::Result<()> {
    check()
}

fn missing(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, what.to_string())
}

#[cfg(target_os = "macos")]
fn check() -> io::Result<()> {
    unsafe extern "C" {
        fn cortex_fuse_t_available() -> std::ffi::c_int;
    }
    // SAFETY: loads libfuse-t and resolves the shim's pointers; calls nothing through them.
    if unsafe { cortex_fuse_t_available() } == 0 {
        return Err(missing(
            "mounting on macOS needs FUSE-T, which is not installed: \
             brew install --cask fuse-t, or the installer from https://www.fuse-t.org",
        ));
    }
    // libfuse-t spawns its server from this compiled-in path, so a missing server is a
    // broken install, not something to point elsewhere.
    if !std::path::Path::new("/usr/local/bin/go-nfsv4").is_file() {
        return Err(missing(
            "FUSE-T is installed without its server (/usr/local/bin/go-nfsv4): \
             reinstall it with brew reinstall --cask fuse-t",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn check() -> io::Result<()> {
    use winapi::um::libloaderapi::LoadLibraryW;

    const INSTALL: &str = "install Dokany 2 from https://github.com/dokan-dev/dokany/releases \
                           (DokanSetup.exe), or winget install --id dokan-dev.Dokany";

    // Loaded rather than searched for, so the loader's search order decides. Left loaded;
    // the mount would load it anyway.
    let name: Vec<u16> = "dokan2.dll\0".encode_utf16().collect();
    // SAFETY: a nul-terminated UTF-16 string that outlives the call.
    if unsafe { LoadLibraryW(name.as_ptr()) }.is_null() {
        return Err(missing(&format!(
            "mounting on Windows needs Dokany, and dokan2.dll is not installed: {INSTALL}"
        )));
    }
    // The DLL is only the user-mode half and may be bundled on a host that never installed
    // the driver.
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let driver = std::path::Path::new(&root).join(r"System32\drivers\dokan2.sys");
    if !driver.is_file() {
        return Err(missing(&format!(
            "mounting on Windows needs the Dokany driver, and {} is not installed: {INSTALL}",
            driver.display()
        )));
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn check() -> io::Result<()> {
    if !std::path::Path::new("/dev/fuse").exists() {
        return Err(missing(
            "mounting needs /dev/fuse, which this host does not have: load the module with \
             modprobe fuse, or start the container with --device /dev/fuse",
        ));
    }
    // Non-root users mount through the setuid helper; `fuser` tries `fusermount3`, then
    // `fusermount`.
    // SAFETY: `geteuid` cannot fail and touches no memory.
    let root = unsafe { libc::geteuid() } == 0;
    let on_path = |name: &str| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
    };
    if !root && !on_path("fusermount3") && !on_path("fusermount") {
        return Err(missing(
            "mounting as a user other than root needs fusermount3, which is not installed: \
             install fuse3 (apt install fuse3, dnf install fuse3)",
        ));
    }
    Ok(())
}
