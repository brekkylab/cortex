//! Whether this host can mount at all, and what to install when it cannot.
//!
//! **A build with `mount` runs on a host without the provider.** On macOS the shim imports
//! libfuse-t weakly, and on Windows the binaries this repository links load `dokan2.dll`
//! only when a mount first calls into it. So what a missing provider costs is a mount, not
//! the process -- and this is where it is found out, before a binding calls into a library
//! that is not there.
//!
//! On Windows that holds only for a binary linked with `/DELAYLOAD:dokan2.dll`, which a
//! `rustc-link-arg` cannot ask for on a dependent's behalf; see the `mount` feature in
//! `Cargo.toml`. A binary linked without it still needs the DLL to start, and never gets as
//! far as calling this.

use std::io;

/// Check this host has what its `mount` binding needs at run time, and if not, say what to
/// install.
///
/// Every binding's `try_new` calls this first, so a caller that only wants to mount has no
/// need to; it is for one that wants to know ahead -- to hide a feature, or to tell a user
/// before they have asked for a mount.
///
/// | Target | Checked | Otherwise |
/// |---|---|---|
/// | macOS | libfuse-t loaded, of a FUSE-T release the shim is checked for (1.x), and the NFS server it starts is where it starts it from | `brew install --cask fuse-t`, or the release named |
/// | Windows | `dokan2.dll` loads, and the `dokan2.sys` driver is installed | the Dokany 2 installer |
/// | Linux | `/dev/fuse`, and `fusermount3` or `fusermount` for a user that is not root | `fuse3` from the distribution |
///
/// `Ok` is a host that has the pieces, not a promise that a mount will succeed: the mount
/// point, the driver's own state and the user's rights are still the mount's to find out.
/// The error is [`io::ErrorKind::Unsupported`], with the installation in its message.
pub fn mount_support() -> io::Result<()> {
    check()
}

fn missing(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, what.to_string())
}

#[cfg(target_os = "macos")]
fn check() -> io::Result<()> {
    use std::ffi::{CStr, c_char, c_int};

    // What `contrib/fuse_t/shim.h` answers.
    const OK: c_int = 1;
    const OTHER_API: c_int = 2;
    const OTHER_MAJOR: c_int = 3;
    unsafe extern "C" {
        fn cortex_fuse_t_status() -> c_int;
        fn cortex_fuse_t_api() -> c_int;
        fn cortex_fuse_t_release() -> *const c_char;
        fn cortex_fuse_t_checked() -> *const c_char;
    }
    // SAFETY: each answers what the shim found when it opened libfuse-t, once, and calls
    // nothing in the library but `fuse_version`; the strings are the shim's own, static.
    let (status, api, release, checked) = unsafe {
        (
            cortex_fuse_t_status(),
            cortex_fuse_t_api(),
            CStr::from_ptr(cortex_fuse_t_release()).to_string_lossy(),
            CStr::from_ptr(cortex_fuse_t_checked()).to_string_lossy(),
        )
    };
    // A libfuse-t whose layouts the shim's declarations may not match is refused before a
    // call could crash on it -- and with the release to install instead.
    let install = format!(
        "install FUSE-T {checked} from https://github.com/macos-fuse-t/fuse-t/releases/tag/{checked}, \
         or set CORTEX_FUSE_T_UNCHECKED=1 to mount with this one anyway, at the risk of a crash"
    );
    match status {
        OK => {}
        OTHER_API => {
            return Err(missing(&format!(
                "the installed FUSE-T speaks libfuse API {api}, and cortex is built for \
                 libfuse 2 (26 to 29): {install}"
            )));
        }
        OTHER_MAJOR => {
            return Err(missing(&format!(
                "FUSE-T {release} is installed, and cortex is checked against FUSE-T 1.x \
                 (last {checked}): {install}"
            )));
        }
        _ => {
            return Err(missing(
                "mounting on macOS needs FUSE-T, which is not installed: \
                 brew install --cask fuse-t, or the installer from https://www.fuse-t.org",
            ));
        }
    }
    // libfuse-t serves every mount through a server it spawns from this path, which is
    // compiled into it -- so a library without the server is an install that went wrong
    // rather than one this could point elsewhere.
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

    // Loaded rather than looked for: the search order is the loader's, and a DLL found on
    // disk that the loader would not pick is not one a mount can use. Left loaded, since
    // the first call a mount makes loads it anyway.
    let name: Vec<u16> = "dokan2.dll\0".encode_utf16().collect();
    // SAFETY: a nul-terminated UTF-16 string that outlives the call.
    if unsafe { LoadLibraryW(name.as_ptr()) }.is_null() {
        return Err(missing(&format!(
            "mounting on Windows needs Dokany, and dokan2.dll is not installed: {INSTALL}"
        )));
    }
    // The library is only the user-mode half; the volume is the driver's, and the DLL can
    // be present -- bundled beside a program, say -- on a host that never ran the installer.
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
    // Only root mounts directly; anyone else goes through the setuid helper, as `fuser`
    // does, trying `fusermount3` and then `fusermount`.
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
