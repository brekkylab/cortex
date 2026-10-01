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
/// | macOS | libfuse-t loaded, of a FUSE-T release the shim is checked for (1.x), and the NFS server it starts is where it starts it from | `brew install --cask fuse-t`, or the release named |
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
    use std::ffi::{CStr, c_char, c_int};

    // What `contrib/fuse_t/shim.h` answers.
    const OK: c_int = 1;
    const OTHER_API: c_int = 2;
    const OTHER_MAJOR: c_int = 3;
    unsafe extern "C" {
        fn virtx_fuse_t_status() -> c_int;
        fn virtx_fuse_t_api() -> c_int;
        fn virtx_fuse_t_release() -> *const c_char;
        fn virtx_fuse_t_checked() -> *const c_char;
    }
    // SAFETY: each reports what the shim found opening libfuse-t once, calling nothing in it
    // but `fuse_version`; the strings are the shim's own statics.
    let (status, api, release, checked) = unsafe {
        (
            virtx_fuse_t_status(),
            virtx_fuse_t_api(),
            CStr::from_ptr(virtx_fuse_t_release()).to_string_lossy(),
            CStr::from_ptr(virtx_fuse_t_checked()).to_string_lossy(),
        )
    };
    // A libfuse-t the shim's declarations may not match is refused before a call could crash
    // on it, naming the release to install instead.
    let install = format!(
        "install FUSE-T {checked} from https://github.com/macos-fuse-t/fuse-t/releases/tag/{checked}, \
         or set VIRTX_FUSE_T_UNCHECKED=1 to mount with this one anyway, at the risk of a crash"
    );
    match status {
        OK => {}
        OTHER_API => {
            return Err(missing(&format!(
                "the installed FUSE-T speaks libfuse API {api}, and virtx is built for \
                 libfuse 2 (26 to 29): {install}"
            )));
        }
        OTHER_MAJOR => {
            return Err(missing(&format!(
                "FUSE-T {release} is installed, and virtx is checked against FUSE-T 1.x \
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
