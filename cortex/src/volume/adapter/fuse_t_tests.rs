//! The FUSE-T binding, driven as `contrib/fuse_t/shim.c` drives it — no libfuse,
//! session, mount or kernel, since each callback just fills out-params and
//! returns an errno.
//!
//! Calls go through [`ops_for`] rather than naming the functions, so the vtable's
//! field order — the contract with `struct cortex_fuse_t_ops` — is covered too.
//! Only this binding's own decisions are checked; inode identity, handle lifetime
//! and the attribute *values* are pinned in `posix.rs`.

use std::sync::Arc;

use super::*;
use crate::volume::{DirentKind, FileExt, InMemVolume, OpenOptions};

const CONTENT: &[u8] = b"Hello from cortex!\n";
const ROOT: u64 = 1;

/// From `libc`, as `HOST_OPEN_FLAGS` is: hand-written numbers would only agree
/// with themselves.
const O_RDONLY: c_int = libc::O_RDONLY;
const O_WRONLY: c_int = libc::O_WRONLY;

/// Held as [`FuseTMount`] holds it: the box never moves, so the pointer stays
/// valid.
struct Shim {
    /// `Arc` because `PosixFs` consumes its backend; one test needs to reach past
    /// the vtable, whose `*const c_char` cannot carry the name it plants.
    vol: Arc<InMemVolume>,
    fs: Box<PosixFs<Arc<InMemVolume>>>,
    ops: Ops,
}

impl Shim {
    /// `greeting.txt` at the root plus an empty `sub/`.
    fn new() -> Self {
        let vol = Arc::new(InMemVolume::new());
        super::super::block_on(async {
            let (file, _) = vol
                .open(Path::new("greeting.txt"), OpenOptions::create_new())
                .await
                .unwrap();
            file.write_all_at(CONTENT, 0).await.unwrap();
            vol.mkdir(Path::new("sub")).await.unwrap();
        });
        Shim {
            fs: Box::new(PosixFs::new(Arc::clone(&vol))),
            ops: ops_for::<Arc<InMemVolume>>(),
            vol,
        }
    }

    fn ptr(&self) -> *mut c_void {
        &*self.fs as *const PosixFs<Arc<InMemVolume>> as *mut c_void
    }

    fn lookup(&self, parent: u64, name: &str) -> (c_int, u64, CortexStat) {
        let name = CString::new(name).unwrap();
        let mut inode = 0;
        let mut stat = CortexStat::default();
        let rc =
            unsafe { (self.ops.lookup)(self.ptr(), parent, name.as_ptr(), &mut inode, &mut stat) };
        (rc, inode, stat)
    }

    fn getattr(&self, inode: u64) -> (c_int, CortexStat) {
        let mut stat = CortexStat::default();
        let rc = unsafe { (self.ops.getattr)(self.ptr(), inode, &mut stat) };
        (rc, stat)
    }

    /// `size` carries its own validity flag, because `0` is a real size to ask for.
    fn setattr(&self, inode: u64, size: Option<u64>) -> (c_int, CortexStat) {
        let mut stat = CortexStat::default();
        let rc = unsafe {
            (self.ops.setattr)(
                self.ptr(),
                inode,
                0,
                0,
                size.unwrap_or(99),
                c_int::from(size.is_some()),
                &mut stat,
            )
        };
        (rc, stat)
    }

    fn open(&self, inode: u64, flags: c_int) -> (c_int, u64) {
        let mut fh = 0;
        let rc = unsafe { (self.ops.open)(self.ptr(), inode, flags, &mut fh) };
        (rc, fh)
    }

    fn create(&self, parent: u64, name: &str, flags: c_int) -> (c_int, u64, u64, CortexStat) {
        let name = CString::new(name).unwrap();
        let (mut inode, mut fh) = (0, 0);
        let mut stat = CortexStat::default();
        let rc = unsafe {
            (self.ops.create)(
                self.ptr(),
                parent,
                name.as_ptr(),
                flags,
                &mut inode,
                &mut fh,
                &mut stat,
            )
        };
        (rc, inode, fh, stat)
    }

    /// Into a caller-allocated buffer, as the shim does.
    fn read(&self, fh: u64, offset: u64, size: u64) -> (c_long, Vec<u8>) {
        let mut buf = vec![0u8; size as usize];
        let n = unsafe {
            (self.ops.read)(
                self.ptr(),
                fh,
                offset,
                size,
                buf.as_mut_ptr() as *mut c_char,
            )
        };
        if n > 0 {
            buf.truncate(n as usize);
        } else {
            buf.clear();
        }
        (n, buf)
    }

    fn write(&self, fh: u64, offset: u64, data: &[u8]) -> c_long {
        unsafe {
            (self.ops.write)(
                self.ptr(),
                fh,
                offset,
                data.len() as u64,
                data.as_ptr() as *const c_char,
            )
        }
    }

    fn mkdir(&self, parent: u64, name: &str) -> (c_int, u64, CortexStat) {
        let name = CString::new(name).unwrap();
        let mut inode = 0;
        let mut stat = CortexStat::default();
        let rc =
            unsafe { (self.ops.mkdir)(self.ptr(), parent, name.as_ptr(), &mut inode, &mut stat) };
        (rc, inode, stat)
    }

    fn unlink(&self, parent: u64, name: &str) -> c_int {
        let name = CString::new(name).unwrap();
        unsafe { (self.ops.unlink)(self.ptr(), parent, name.as_ptr()) }
    }

    fn rmdir(&self, parent: u64, name: &str) -> c_int {
        let name = CString::new(name).unwrap();
        unsafe { (self.ops.rmdir)(self.ptr(), parent, name.as_ptr()) }
    }

    fn rename(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> c_int {
        let (name, newname) = (CString::new(name).unwrap(), CString::new(newname).unwrap());
        unsafe {
            (self.ops.rename)(
                self.ptr(),
                parent,
                name.as_ptr(),
                newparent,
                newname.as_ptr(),
            )
        }
    }

    fn readdir(&self, inode: u64, offset: u64, sink: &mut Collected) -> c_int {
        unsafe {
            (self.ops.readdir)(
                self.ptr(),
                inode,
                offset,
                sink as *mut Collected as *mut c_void,
                collect,
            )
        }
    }

    fn flush(&self, fh: u64) -> c_int {
        unsafe { (self.ops.flush)(self.ptr(), fh) }
    }

    fn release(&self, fh: u64) -> c_int {
        unsafe { (self.ops.release)(self.ptr(), fh) }
    }

    fn forget(&self, inode: u64, nlookup: u64) {
        unsafe { (self.ops.forget)(self.ptr(), inode, nlookup) }
    }
}

/// Stands in for the C side's `fuse_add_direntry` accounting.
#[derive(Default)]
struct Collected {
    rows: Vec<(u64, String, u32, u64)>,
    /// Report "buffer full" after this many rows; `0` never does.
    stop_after: usize,
}

impl Collected {
    fn names(&self) -> Vec<&str> {
        self.rows
            .iter()
            .map(|(_, name, ..)| name.as_str())
            .collect()
    }
}

unsafe extern "C" fn collect(
    sink: *mut c_void,
    inode: u64,
    name: *const c_char,
    mode: u32,
    cursor: u64,
) -> c_int {
    let sink = unsafe { &mut *(sink as *mut Collected) };
    let name = unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned();
    sink.rows.push((inode, name, mode, cursor));
    c_int::from(sink.stop_after != 0 && sink.rows.len() >= sink.stop_after)
}

#[test]
fn a_read_only_walk_through_the_vtable_reaches_the_backend() {
    let shim = Shim::new();

    let (rc, inode, stat) = shim.lookup(ROOT, "greeting.txt");
    assert_eq!(rc, 0);
    assert_eq!(stat.size, CONTENT.len() as u64);
    assert_eq!(
        stat.ino, inode,
        "the shim reads the number out of the struct"
    );

    let (rc, again) = shim.getattr(inode);
    assert_eq!(rc, 0);
    assert_eq!(
        (again.ino, again.size, again.mode),
        (inode, stat.size, stat.mode)
    );

    let (rc, fh) = shim.open(inode, O_RDONLY);
    assert_eq!(rc, 0);

    // The shim allocates `size` and trusts the count, so the count has to match
    // what was written — including short of the buffer, which is how EOF is told.
    let (n, data) = shim.read(fh, 0, CONTENT.len() as u64);
    assert_eq!((n, &data[..]), (CONTENT.len() as c_long, CONTENT));
    let (n, data) = shim.read(fh, 0, 4096);
    assert_eq!((n, &data[..]), (CONTENT.len() as c_long, CONTENT));
    assert_eq!(shim.read(fh, CONTENT.len() as u64, 16).0, 0);
    assert_eq!(shim.read(fh, 6, 4).1, b"from");

    // FLUSH is repeatable and leaves the handle usable; RELEASE ends it.
    assert_eq!(shim.flush(fh), 0);
    assert_eq!(shim.flush(fh), 0);
    assert_eq!(shim.release(fh), 0);
    assert_eq!(shim.flush(fh), -libc::EBADF);

    // FORGET returns nothing, so the next call is the only witness.
    shim.forget(inode, 1);
    assert_eq!(shim.getattr(inode).0, -libc::ENOENT);
}

#[test]
fn the_mutating_callbacks_reach_the_backend() {
    let shim = Shim::new();

    let (rc, inode, fh, stat) = shim.create(ROOT, "fresh.txt", O_WRONLY);
    assert_eq!(rc, 0);
    assert_eq!(stat.size, 0);
    assert_eq!(stat.mode, 0o100000 | 0o644);

    // A raw pointer and a length in; the slice built from them has to be exactly
    // the bytes handed over.
    assert_eq!(shim.write(fh, 0, b"written"), 7);
    assert_eq!(shim.getattr(inode).1.size, 7);
    assert_eq!(shim.write(fh, 7, b"!"), 1);
    assert_eq!(shim.getattr(inode).1.size, 8);

    let (rc, dir, dir_stat) = shim.mkdir(ROOT, "made");
    assert_eq!(rc, 0);
    assert_eq!(dir_stat.mode, 0o040000 | 0o755);
    assert_eq!(dir_stat.nlink, 1);

    // A rename moves a live object, so the same inode answers under the new name.
    assert_eq!(shim.rename(ROOT, "fresh.txt", dir, "moved.txt"), 0);
    assert_eq!(shim.lookup(ROOT, "fresh.txt").0, -libc::ENOENT);
    let (rc, moved, _) = shim.lookup(dir, "moved.txt");
    assert_eq!(rc, 0);
    assert_eq!(moved, inode);
    assert_eq!(shim.getattr(inode).1.size, 8, "and the bytes came along");

    assert_eq!(shim.release(fh), 0);
    assert_eq!(shim.unlink(dir, "moved.txt"), 0);
    assert_eq!(shim.rmdir(ROOT, "made"), 0);
    assert_eq!(shim.lookup(ROOT, "made").0, -libc::ENOENT);
}

/// Every member of `struct cortex_stat` gets a value. One left at zero is not a
/// compile error but a file the kernel thinks has no links, or an mtime in 1970.
#[test]
fn the_cortex_stat_projection_fills_every_field() {
    let known = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let mut stat = Stat::new(DirentKind::File, 1025);
    stat.mtime = Some(known + Duration::from_nanos(123));
    let projected = to_cortex_stat(42, &stat);

    assert_eq!(projected.ino, 42);
    assert_eq!(projected.size, 1025);
    assert_eq!(
        projected.blocks, 3,
        "1025 bytes takes three 512-byte blocks"
    );
    assert_eq!(projected.mode, 0o100000 | 0o644);
    assert_eq!(projected.nlink, 1);
    assert_eq!(projected.blksize as u64, BLOCK_SIZE);
    assert_eq!(projected._pad, 0, "so C can trust the offsets after it");

    // Seconds *and* nanoseconds, on all three — the two the backend does not
    // report follow the one it does.
    let stamp = (1_700_000_000, 123);
    assert_eq!((projected.mtime, projected.mtime_nsec), stamp);
    assert_eq!((projected.atime, projected.atime_nsec), stamp);
    assert_eq!((projected.ctime, projected.ctime_nsec), stamp);

    let dir = to_cortex_stat(1, &Stat::new(DirentKind::Dir, 0));
    assert_eq!(dir.mode, 0o040000 | 0o755);
    assert_eq!(dir.nlink, 1);

    // A narrowing cast, so it has to survive the trip into `u32`.
    assert_eq!(ops_for::<Arc<InMemVolume>>().block_size as u64, BLOCK_SIZE);
}

/// Failures leave as a **negative** errno — this binding's convention alone. A
/// lost minus turns "no such file" into a perfectly good success code, and no
/// reviewer sees it.
#[test]
fn a_failure_is_reported_as_a_negative_host_errno() {
    let shim = Shim::new();

    #[rustfmt::skip]
    let rows = [
        ("missing name",     shim.lookup(ROOT, "nope").0,                              libc::ENOENT),
        ("unissued inode",   shim.getattr(9999).0,                                     libc::ENOENT),
        ("rmdir on a file",  shim.rmdir(ROOT, "greeting.txt"),                         libc::ENOTDIR),
        ("unlink on a dir",  shim.unlink(ROOT, "sub"),                                 libc::EISDIR),
        ("O_EXCL over one",  shim.create(ROOT, "greeting.txt", O_WRONLY | libc::O_EXCL).0, libc::EEXIST),
        ("rename, no source", shim.rename(ROOT, "nope", ROOT, "elsewhere"),            libc::ENOENT),
    ];
    for (what, rc, expected) in rows {
        assert!(rc < 0, "{what} reported {rc}, which reads as success");
        assert_eq!(rc, -expected, "{what}");
    }

    // The `c_long` pair widens the number as well as negating it.
    let closed = 4242;
    assert_eq!(shim.read(closed, 0, 16).0, -(libc::EBADF as c_long));
    assert_eq!(shim.write(closed, 0, b"x"), -(libc::EBADF as c_long));
}

/// The host flag words reach only this binding and `fuser`'s, and `fuser`'s needs
/// a mount — so this is the only place they are pinned.
#[test]
fn the_host_flag_words_decode_into_the_right_options() {
    let shim = Shim::new();
    let (_, inode, _) = shim.lookup(ROOT, "greeting.txt");

    // A plain open leaves the bytes alone.
    let (rc, fh) = shim.open(inode, O_WRONLY);
    assert_eq!(rc, 0);
    assert_eq!(shim.getattr(inode).1.size, CONTENT.len() as u64);
    assert_eq!(shim.release(fh), 0);

    // `O_TRUNC` lands before the handle exists, so the size is already 0.
    let (rc, fh) = shim.open(inode, O_WRONLY | libc::O_TRUNC);
    assert_eq!(rc, 0);
    assert_eq!(shim.getattr(inode).1.size, 0);
    assert_eq!(shim.release(fh), 0);

    // `O_ACCMODE` is a two-bit *field* and 3 is not a value in it — what a
    // `flags & O_WRONLY` test waves through. OPEN and CREATE each decode it.
    assert_eq!(shim.open(inode, 3), (-libc::EINVAL, 0));
    assert_eq!(shim.create(ROOT, "impossible", 3).0, -libc::EINVAL);

    // The CREATE opcode means "make it if absent" whatever the flags say.
    let (rc, _, fh, stat) = shim.create(ROOT, "no-creat-bit", O_WRONLY);
    assert_eq!(rc, 0);
    assert_eq!(stat.size, 0);
    assert_eq!(shim.release(fh), 0);
    assert_eq!(shim.lookup(ROOT, "no-creat-bit").0, 0);

    // SETATTR's size has a separate validity flag, so both readings matter:
    // without it nothing resizes, with it a zero really truncates.
    let (rc, unchanged) = shim.setattr(inode, None);
    assert_eq!((rc, unchanged.size), (0, 0), "no size asked for");
    let (_, fh) = shim.open(inode, O_WRONLY);
    assert_eq!(shim.write(fh, 0, b"refilled"), 8);
    assert_eq!(shim.setattr(inode, Some(3)).1.size, 3);
    assert_eq!(
        shim.setattr(inode, Some(0)).1.size,
        0,
        "zero is a real size"
    );
    assert_eq!(shim.release(fh), 0);
}

#[test]
fn readdir_streams_through_the_sink_and_honours_its_stop() {
    let shim = Shim::new();

    let mut all = Collected::default();
    assert_eq!(shim.readdir(ROOT, 0, &mut all), 0);

    // Dots lead, both pointing back here, and the cursor is 1-based so `0` can
    // keep meaning "from the beginning".
    let dots = mode_for(DirentKind::Dir);
    assert_eq!(all.rows[0], (ROOT, ".".into(), dots, 1));
    assert_eq!(all.rows[1], (ROOT, "..".into(), dots, 2));
    let mut names = all.names();
    names.sort();
    assert_eq!(names, [".", "..", "greeting.txt", "sub"]);

    // Each child arrives with its kind resolved and the number lookup confirms —
    // the sink can ask for neither.
    let file = all
        .rows
        .iter()
        .find(|(_, name, ..)| name == "greeting.txt")
        .unwrap();
    assert_eq!(file.2, mode_for(DirentKind::File));
    assert_eq!(file.0, shim.lookup(ROOT, "greeting.txt").1);

    // The kernel resumes by quoting the last cursor, so 2 skips both dots.
    let mut resumed = Collected::default();
    assert_eq!(shim.readdir(ROOT, 2, &mut resumed), 0);
    let mut names = resumed.names();
    names.sort();
    assert_eq!(names, ["greeting.txt", "sub"]);

    // "Buffer full" is not a failure: stop, but still report success, or the
    // kernel sees an error where it asked for a second round.
    let mut full = Collected {
        stop_after: 1,
        ..Default::default()
    };
    assert_eq!(shim.readdir(ROOT, 0, &mut full), 0);
    assert_eq!(full.names(), ["."]);
}

/// Truncating at the NUL would publish an entry called `a` that no `lookup` can
/// resolve. `CString::new` is the only guard, and this is the crate's only place
/// that turns a listing name back into a C string.
#[test]
fn a_name_with_an_interior_nul_is_refused() {
    let shim = Shim::new();
    // Past the vtable, whose `*const c_char` cannot carry this name.
    super::super::block_on(Mountable::mkdir(&*shim.vol, Path::new("a\0b"))).unwrap();

    let mut sink = Collected::default();
    assert_eq!(shim.readdir(ROOT, 0, &mut sink), -libc::EINVAL);
}
