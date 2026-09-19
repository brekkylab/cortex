//! What a commit takes out of the guest: this session's overlay upperdir, as a tar.
//!
//! # What overlayfs leaves, and what has to be rewritten
//!
//! A file the session wrote is in the upperdir as itself. A file it **deleted** is a character
//! device `0:0`, and that travels through an ordinary tar as itself — `ingest_tar` on the
//! other side reconstructs exactly the whiteout node the merge acts on, so nothing has to be
//! done to it.
//!
//! A directory it **emptied** is marked with a `trusted.overlay.opaque` xattr, and an xattr
//! does not survive an ordinary tar. That one is written as OCI's `.wh..wh..opq` marker inside
//! the directory, which `ingest_tar` turns back into the xattr. It is the only conversion here.
//!
//! # What is left out
//!
//! Everything the console server and this agent put in the session's own filesystem: this
//! binary, the root kept so that this walk is possible at all, the shim directory, the scratch
//! being written into, and the mount points of anything shared in. None of it is the session's
//! work, and one of them is this binary — over a megabyte of it.
//!
//! **A mount point's ancestors go too, when they were only ever the way to it.** The context is
//! mounted at the host's own path, so a session on `/private/var/folders/…/T/x` has every
//! directory along that path in its upper, created for no other reason. Dropping the first
//! component would be wrong in general — a tree at `/home/me/proj` must not take `/home` with
//! it — so what is dropped is an ancestor with nothing else beneath it.

use std::io;
use std::path::{Path, PathBuf};

/// The xattr overlayfs marks an emptied directory with.
const OPAQUE_XATTR: &[u8] = b"trusted.overlay.opaque\0";

/// What OCI calls one, which is a thing a tar can carry.
const OPAQUE_MARKER: &str = ".wh..wh..opq";

/// What a walk found: a path relative to the upperdir, and what kind of thing it is.
struct Found {
    path: PathBuf,
    kind: Kind,
}

/// Only the three that matter here. A whiteout is the reason [`Kind::Device`] exists.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Directory,
    /// A regular file or a symlink — anything `tar` can add from a path.
    Ordinary,
    /// A character or block device, which for our purposes is a deletion.
    Device,
}

/// Write everything under `upper` into a tar at `into`, and say how big it came out.
///
/// `excluded` and `mount_points` are both paths relative to `upper` and both skipped whole.
/// They differ in what happens to what is *above* them: a mount point's ancestors are dropped
/// when nothing else is left in them, because those directories exist only to reach it.
pub fn write(
    upper: &Path,
    into: &Path,
    excluded: &[PathBuf],
    mount_points: &[PathBuf],
) -> io::Result<u64> {
    let mut found = Vec::new();
    let skip: Vec<&PathBuf> = excluded.iter().chain(mount_points).collect();
    collect(upper, Path::new(""), &skip, &mut found)?;
    prune_ancestors(&mut found, mount_points);

    let file = std::fs::File::create(into)?;
    let mut builder = tar::Builder::new(file);
    // Not followed: a symlink in the upper is a symlink the session made, and following one
    // would put whatever it names into the layer.
    builder.follow_symlinks(false);

    for entry in &found {
        let full = upper.join(&entry.path);

        // Named, because a failure here is nearly always about one path and the archive
        // writer's own message does not say which.
        let named = |e: io::Error| {
            io::Error::other(format!("adding {} to the layer: {e}", entry.path.display()))
        };

        match entry.kind {
            // Written by hand rather than through `append_path_with_name`, which for a
            // special file ignores the name it is given and uses the filesystem path — so a
            // whiteout would go into the archive as `/oldroot/mnt/upper/upper/etc/motd` and be
            // refused for being absolute. Writing the header also means saying exactly what a
            // whiteout is rather than hoping it is reproduced.
            Kind::Device => {
                let mut header = tar::Header::new_gnu();
                let (major, minor) = device_numbers(&full)?;
                header.set_entry_type(if major == 0 && minor == 0 {
                    tar::EntryType::Char
                } else {
                    device_entry_type(&full)?
                });
                header.set_mode(0o644);
                header.set_size(0);
                header.set_device_major(major).map_err(named)?;
                header.set_device_minor(minor).map_err(named)?;
                builder
                    .append_data(&mut header, &entry.path, io::empty())
                    .map_err(named)?;
            }
            Kind::Directory | Kind::Ordinary => {
                builder
                    .append_path_with_name(&full, &entry.path)
                    .map_err(named)?;
            }
        }

        if entry.kind == Kind::Directory && is_opaque(&full) {
            // Inside the directory it empties, which is where OCI puts it and where
            // `ingest_tar` looks.
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(0);
            builder
                .append_data(&mut header, entry.path.join(OPAQUE_MARKER), io::empty())
                .map_err(named)?;
        }
    }

    let file = builder.into_inner()?;
    file.sync_all()?;
    Ok(file.metadata()?.len())
}

/// Everything under `dir`, in name order, skipping `skip`.
///
/// Sorted so that one upperdir gives one tar: the layer is named by the digest of these bytes,
/// and a directory read twice in two orders would be two layers holding the same thing.
fn collect(
    upper: &Path,
    relative: &Path,
    skip: &[&PathBuf],
    found: &mut Vec<Found>,
) -> io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(upper.join(relative))?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    use std::os::unix::fs::FileTypeExt as _;

    for entry in entries {
        let path = if relative.as_os_str().is_empty() {
            PathBuf::from(entry.file_name())
        } else {
            relative.join(entry.file_name())
        };
        if skip.iter().any(|excluded| path.starts_with(excluded)) {
            continue;
        }

        let metadata = upper.join(&path).symlink_metadata()?;
        let file_type = metadata.file_type();

        let kind = if metadata.is_dir() {
            Kind::Directory
        } else if file_type.is_char_device() || file_type.is_block_device() {
            Kind::Device
        } else if file_type.is_socket() || file_type.is_fifo() {
            // A socket is the shim's, and the only one here; `tar` refuses one outright. A
            // fifo in a session's upper is not something to carry into an image either.
            continue;
        } else {
            Kind::Ordinary
        };

        found.push(Found {
            path: path.clone(),
            kind,
        });
        if kind == Kind::Directory {
            collect(upper, &path, skip, found)?;
        }
    }
    Ok(())
}

/// Drop the directories that existed only to reach a mount point.
///
/// An ancestor stays if anything else survived under it, which is what tells
/// `/private/var/folders/…` — created for the share and nothing else — from `/home` on a host
/// where the tree happens to live under one.
fn prune_ancestors(found: &mut Vec<Found>, mount_points: &[PathBuf]) {
    let mut doomed: Vec<PathBuf> = Vec::new();

    for mount in mount_points {
        for ancestor in mount.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                continue;
            }
            let has_other = found.iter().any(|entry| {
                entry.path != ancestor
                    && entry.path.starts_with(ancestor)
                    && !doomed.iter().any(|gone| entry.path.starts_with(gone))
            });
            if has_other {
                // Something real is under here, so this directory is not only a way to the
                // mount point — and neither is anything above it.
                break;
            }
            doomed.push(ancestor.to_path_buf());
        }
    }

    found.retain(|entry| !doomed.iter().any(|gone| entry.path.starts_with(gone)));
}

/// A device node's major and minor numbers.
fn device_numbers(path: &Path) -> io::Result<(u32, u32)> {
    use std::os::unix::fs::MetadataExt as _;

    let rdev = path.symlink_metadata()?.rdev();
    Ok((libc::major(rdev), libc::minor(rdev)))
}

/// Character or block, for a device that is not a whiteout.
fn device_entry_type(path: &Path) -> io::Result<tar::EntryType> {
    use std::os::unix::fs::FileTypeExt as _;

    let file_type = path.symlink_metadata()?.file_type();
    Ok(if file_type.is_block_device() {
        tar::EntryType::Block
    } else {
        tar::EntryType::Char
    })
}

/// Whether overlayfs marked this directory as hiding what is below it.
fn is_opaque(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    let mut value = [0u8; 1];
    // SAFETY: `path` is NUL-terminated and outlives the call, `OPAQUE_XATTR` is a NUL-
    // terminated literal, and the length matches the buffer handed over.
    let read = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            OPAQUE_XATTR.as_ptr() as *const libc::c_char,
            value.as_mut_ptr() as *mut libc::c_void,
            value.len(),
        )
    };
    read > 0 && value[0] == b'y'
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn names(tar: &Path) -> Vec<String> {
        let mut archive = tar::Archive::new(std::fs::File::open(tar).unwrap());
        archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().display().to_string())
            .collect()
    }

    fn kinds(tar: &Path) -> Vec<(String, tar::EntryType)> {
        let mut archive = tar::Archive::new(std::fs::File::open(tar).unwrap());
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.path().unwrap().display().to_string(),
                    entry.header().entry_type(),
                )
            })
            .collect()
    }

    #[test]
    fn a_layer_carries_what_the_session_wrote() {
        let upper = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(upper.path().join("etc")).unwrap();
        std::fs::write(upper.path().join("etc/added"), b"new\n").unwrap();
        let exe = upper.path().join("runnable");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("etc/added", upper.path().join("alias")).unwrap();

        let out = tempfile::tempdir().unwrap();
        let tar = out.path().join("layer.tar");
        let size = write_layer(upper.path(), &tar, &[], &[]).unwrap();
        assert!(size > 0);

        let kinds = kinds(&tar);
        let names: Vec<&str> = kinds.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"etc/added"), "{names:?}");
        assert!(names.contains(&"runnable"), "{names:?}");
        assert!(
            kinds
                .iter()
                .any(|(name, kind)| name == "alias" && *kind == tar::EntryType::Symlink),
            "the symlink was followed or lost: {kinds:?}"
        );
    }

    #[test]
    fn what_was_excluded_is_not_in_it() {
        let upper = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(upper.path().join("oldroot/deep")).unwrap();
        std::fs::write(upper.path().join("oldroot/deep/thing"), b"x").unwrap();
        std::fs::write(upper.path().join("keep"), b"y").unwrap();

        let out = tempfile::tempdir().unwrap();
        let tar = out.path().join("layer.tar");
        write_layer(upper.path(), &tar, &[PathBuf::from("oldroot")], &[]).unwrap();

        let names = names(&tar);
        assert!(names.iter().any(|name| name == "keep"), "{names:?}");
        assert!(
            !names.iter().any(|name| name.starts_with("oldroot")),
            "{names:?}"
        );
    }

    /// The context case: every directory on the way to the mount point was created for it and
    /// goes with it, but a directory that holds something else stays.
    #[test]
    fn a_mount_points_ancestors_go_only_when_they_held_nothing_else() {
        let upper = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(upper.path().join("private/var/folders/T/tmpXY")).unwrap();
        std::fs::create_dir_all(upper.path().join("home/me/proj")).unwrap();
        std::fs::write(upper.path().join("home/me/notes"), b"mine").unwrap();

        let out = tempfile::tempdir().unwrap();
        let tar = out.path().join("layer.tar");
        write_layer(
            upper.path(),
            &tar,
            &[],
            &[
                PathBuf::from("private/var/folders/T/tmpXY"),
                PathBuf::from("home/me/proj"),
            ],
        )
        .unwrap();

        let names = names(&tar);
        assert!(
            !names.iter().any(|name| name.starts_with("private")),
            "the path to the mount point stayed: {names:?}"
        );
        assert!(
            names.iter().any(|name| name == "home/me/notes"),
            "a real file was pruned with the mount point: {names:?}"
        );
        assert!(
            names.iter().any(|name| name == "home/me"),
            "a directory holding something else was pruned: {names:?}"
        );
        assert!(
            !names.iter().any(|name| name == "home/me/proj"),
            "the mount point itself stayed: {names:?}"
        );
    }

    #[test]
    fn a_directory_the_session_made_is_kept_even_when_empty() {
        let upper = tempfile::tempdir().unwrap();
        std::fs::create_dir(upper.path().join("srv")).unwrap();

        let out = tempfile::tempdir().unwrap();
        let tar = out.path().join("layer.tar");
        write_layer(upper.path(), &tar, &[], &[]).unwrap();

        assert!(names(&tar).iter().any(|name| name == "srv"));
    }

    /// An emptied directory is the one thing that has to be rewritten: the xattr overlayfs
    /// uses does not survive a tar, so a marker does instead.
    ///
    /// Setting a `trusted.*` xattr needs `CAP_SYS_ADMIN`. This crate is only ever built for
    /// Linux and only ever runs as root inside a guest, so the test runs where the code does —
    /// and is skipped rather than failed where it cannot.
    #[test]
    fn an_opaque_directory_becomes_a_marker() {
        let upper = tempfile::tempdir().unwrap();
        let emptied = upper.path().join("media");
        std::fs::create_dir(&emptied).unwrap();
        if !set_opaque(&emptied) {
            eprintln!("skipping: setting trusted.overlay.opaque needs CAP_SYS_ADMIN");
            return;
        }

        let out = tempfile::tempdir().unwrap();
        let tar = out.path().join("layer.tar");
        write_layer(upper.path(), &tar, &[], &[]).unwrap();

        let names = names(&tar);
        assert!(
            names.iter().any(|name| name == "media/.wh..wh..opq"),
            "{names:?}"
        );
    }

    fn set_opaque(dir: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt as _;

        let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).unwrap();
        // SAFETY: both strings are NUL-terminated and outlive the call.
        let rc = unsafe {
            libc::lsetxattr(
                path.as_ptr(),
                OPAQUE_XATTR.as_ptr() as *const libc::c_char,
                b"y".as_ptr() as *const libc::c_void,
                1,
                0,
            )
        };
        rc == 0
    }
}
