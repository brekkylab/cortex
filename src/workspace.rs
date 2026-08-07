//! A namespace that stitches several backends into one filesystem.
//!
//! A [`Workspace`] is a longest-prefix mount table: each backend is registered
//! at a root-relative path and serves every request that falls under it, with
//! the path re-based onto the backend's own root. Because the backends have
//! different concrete `Handle` types, they are stored as
//! [`DynMountable`](crate::DynMountable) trait objects.
//!
//! A `Workspace` is itself a [`Mountable`] (its handle is `Box<dyn FileHandle>`),
//! so it can be driven by the same adapters as any single backend — and even
//! mounted inside another workspace.

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Unbounded};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use async_trait::async_trait;

use crate::{
    CortexError, Dirent, DirentKind, DynMountable, FileExt, FileHandle, FsEvent, FsHook, Mountable,
    OpenOptions, Result, Stat,
};

/// A longest-prefix mount table over heterogeneous backends.
pub struct Workspace {
    /// Mount points keyed by their normalized, root-relative path.
    ///
    /// `PathBuf`'s component-wise `Ord` guarantees that, among all keys that are
    /// a prefix of a request, the longest is also the lexicographically greatest
    /// — so a longest-prefix lookup is a reverse range scan (see
    /// [`Workspace::route`]), and all keys sharing a prefix form one contiguous
    /// run (see [`Workspace::descendant_mounts`]). A backend mounted at the empty
    /// path is the root and serves anything no deeper mount claims.
    mounts: BTreeMap<PathBuf, Box<dyn DynMountable>>,

    /// The timestamp every synthesized directory reports.
    ///
    /// Fixed at construction rather than read per call. A directory that
    /// advertises `SystemTime::now()` on each `stat` looks perpetually modified,
    /// and a guest that negotiated `AUTO_INVAL_DATA` watches exactly that field to
    /// decide when to drop cached pages — so a moving mtime would invalidate
    /// forever. Leaving it unset is no better: it falls back to the UNIX epoch,
    /// which `find -newer`, `make` and `rsync` all read.
    born: SystemTime,

    /// Fired on every mutation this workspace lands. `None` fires nothing. The
    /// host attaches one to react to writes (ingestion, indexing, cache
    /// invalidation) without cortex knowing what the reaction is — see [`FsHook`].
    hook: Option<Arc<dyn FsHook>>,
}

impl Workspace {
    /// An empty workspace with no mounts.
    ///
    /// The root is still a directory — an empty one, like a freshly mounted
    /// tmpfs — so an empty workspace can be mounted and filled in rather than
    /// failing at the kernel's first `getattr`. Every other path is
    /// [`CortexError::NotFound`] until something claims it.
    pub fn new() -> Self {
        Self {
            mounts: BTreeMap::new(),
            born: SystemTime::now(),
            hook: None,
        }
    }

    /// Attach an [`FsHook`] fired on every mutation (create/modify/remove). An
    /// unset hook fires nothing.
    pub fn with_hook(mut self, hook: Option<Arc<dyn FsHook>>) -> Self {
        self.hook = hook;
        self
    }

    /// Fire the attached hook, if any.
    fn fire(&self, event: FsEvent<'_>) {
        if let Some(h) = &self.hook {
            h.on_change(event);
        }
    }

    /// Build a live [`Workspace`] from a serialized [`WorkspaceSpec`]: each mount
    /// realized via [`VolumeSpec::build_mountable`](crate::VolumeSpec::build_mountable),
    /// in the spec's order. This is how a spec carried across a process boundary
    /// (see [`WorkspaceSpec`]) becomes an identical *namespace* on the far side.
    ///
    /// Restores the namespace only — the two instance-local fields are not in the
    /// spec, by design:
    /// - [`hook`](Self::with_hook) cannot be: it is arbitrary host behaviour
    ///   (closing over live channels, indexers) that no serialization can carry to
    ///   another process, and the far side has nothing to react with anyway. The
    ///   process that owns the reaction attaches it here: `from_spec(&spec)?
    ///   .with_hook(hook)`.
    /// - `born` is left fresh. Its job is to stay fixed across *one* instance's
    ///   lifetime (so a guest's `AUTO_INVAL_DATA` cache does not churn), which a
    ///   per-build timestamp already satisfies; only the synthesized-directory
    ///   timestamps differ between two builds of the same spec, and each build is
    ///   self-consistent.
    pub fn from_spec(spec: &crate::WorkspaceSpec) -> Result<Self> {
        let mut ws = Workspace::new();
        for (path, volume) in &spec.mounts {
            ws.mounts
                .insert(mount_key(Path::new(path))?, volume.build_mountable()?);
        }
        Ok(ws)
    }

    /// Builder-style mount that overwrites any backend already at `path`.
    ///
    /// Fails only if `path` escapes the workspace root.
    pub fn try_with_mount<M>(mut self, path: impl AsRef<Path>, backend: M) -> Result<Self>
    where
        M: Mountable + 'static,
        M::Handle: 'static,
    {
        self.mounts
            .insert(mount_key(path.as_ref())?, Box::new(backend));
        Ok(self)
    }

    /// Mount `backend` at `path` (root-relative). Fails if the path escapes the
    /// workspace root or another backend is already mounted there.
    pub fn mount<M>(&mut self, path: impl AsRef<Path>, backend: M) -> Result<()>
    where
        M: Mountable + 'static,
        M::Handle: 'static,
    {
        let key = mount_key(path.as_ref())?;
        if self.mounts.contains_key(&key) {
            return Err(CortexError::AlreadyExists);
        }
        self.mounts.insert(key, Box::new(backend));
        Ok(())
    }

    /// Remove the mount registered at `path`, returning the detached backend.
    pub fn unmount(&mut self, path: impl AsRef<Path>) -> Result<Box<dyn DynMountable>> {
        let key = mount_key(path.as_ref())?;
        self.mounts.remove(&key).ok_or(CortexError::NotFound)
    }

    /// The backend that owns `key` (longest-prefix match), with `key` re-based
    /// onto that backend's mount point. `None` when no mount claims it.
    ///
    /// `key` must already be normalized — the caller does that once and reuses it
    /// for the mount-table queries alongside this one. That is a correctness
    /// requirement, not a convenience: `PosixFs` walks the tree as `/`-rooted
    /// paths, so a stray `RootDir` component would miss every mount.
    ///
    /// `Option` rather than `Result` on purpose. Every caller has to tell "no
    /// mount claims this" apart from "a mount claimed it and answered
    /// `NotFound`", because only the first is a candidate for a directory
    /// synthesized from the mount table. A `Result` here invites `?`, which would
    /// propagate `NotFound` before that decision is made.
    fn route(&self, key: &Path) -> Option<(&dyn DynMountable, PathBuf)> {
        // Walk every key <= `key` from the greatest downward; the first that is a
        // prefix of `key` is the longest match.
        let (mount, backend) = self
            .mounts
            .range(..=key.to_path_buf())
            .rev()
            .find(|(k, _)| key.starts_with(k))?;
        let sub = key
            .strip_prefix(mount)
            .expect("matched mount is a prefix of the request");
        Some((backend.as_ref(), sub.to_path_buf()))
    }

    /// Every mount *strictly* below `prefix`.
    ///
    /// One contiguous run of the map, not a scan: component-wise `Ord` puts
    /// `["a"] < ["a","b"]` (equal prefix, shorter first) and `["a","z"] < ["b"]`
    /// (they diverge at the first component), so keys sharing a prefix are
    /// adjacent and a sibling like `ab` cannot interleave.
    ///
    /// The bound is `Excluded`, so a mount is not below itself. Both callers share
    /// this one range for that reason — an `Included` bound here would make a
    /// backend that answers `NotFound` at its own root look like a synthesized
    /// directory, turning a misconfigured mount into a silently empty one.
    fn descendant_mounts<'a>(
        &'a self,
        prefix: &'a Path,
    ) -> impl Iterator<Item = (&'a PathBuf, &'a Box<dyn DynMountable>)> {
        self.mounts
            .range((Excluded(prefix.to_path_buf()), Unbounded))
            .take_while(move |(k, _)| k.starts_with(prefix))
    }

    /// The child names of `prefix` that come from the mount table, each flagged
    /// with whether a mount sits at *exactly* that path.
    ///
    /// The flag is what decides who wins a name collision with the backend: an
    /// exact child is a mount point and shadows whatever the backend had there,
    /// while a name that merely leads to a deeper mount keeps the backend's entry.
    fn mount_children(&self, prefix: &Path) -> BTreeMap<String, bool> {
        let mut out = BTreeMap::new();
        for (mount, _) in self.descendant_mounts(prefix) {
            let rest = mount
                .strip_prefix(prefix)
                .expect("the range only yields keys prefixed by `prefix`");
            let mut components = rest.components();
            let Some(first) = components.next() else {
                continue;
            };
            let name = first.as_os_str().to_string_lossy().into_owned();
            // One component left over means the mount is at this child itself.
            let exact = components.next().is_none();
            *out.entry(name).or_insert(false) |= exact;
        }
        out
    }

    /// Whether any mount lies strictly below `prefix` — i.e. whether `prefix` is
    /// on the way to one, and so must present as a directory whether or not a
    /// backend knows about it.
    fn spans_mounts(&self, prefix: &Path) -> bool {
        self.descendant_mounts(prefix).next().is_some()
    }

    /// The mount point that owns `key`, if any.
    ///
    /// Separate from [`route`](Self::route) because a move has to compare *which*
    /// mount each of its two paths belongs to. Comparing the resolved backends
    /// instead would work by pointer identity, which is a far more fragile thing to
    /// rest a correctness decision on.
    fn mount_for(&self, key: &Path) -> Option<&Path> {
        self.mounts
            .range(..=key.to_path_buf())
            .rev()
            .find(|(mount, _)| key.starts_with(mount))
            .map(|(mount, _)| mount.as_path())
    }

    /// Whether `key` belongs to the mount table rather than to a backend — a mount
    /// point itself, or a directory that exists only because mounts lie below it.
    ///
    /// Neither is the filesystem's to move. A mount point is *not* caught by
    /// [`is_synthesized_dir`](Self::is_synthesized_dir): nothing lies below it, so
    /// it is a real path its own backend serves — it just is not one a `rename` may
    /// relocate, because doing so would rewrite the mount table through a file
    /// operation.
    fn is_mount_table_owned(&self, key: &Path) -> bool {
        self.mounts.contains_key(key) || self.is_synthesized_dir(key)
    }

    /// Whether `key` is a directory that exists only because of the mount table.
    ///
    /// Either mounts lie below it — so it is on the way to one and must present as
    /// a directory whether or not a backend knows about it — or it is the root of
    /// a workspace with no mounts at all, which is an *empty* directory rather
    /// than a missing one, the way a freshly mounted tmpfs is.
    ///
    /// Deliberately not "the root, always": a workspace whose root backend cannot
    /// stat its own root is misconfigured, and there is no mount table to
    /// synthesize from, so that error surfaces instead of being papered over.
    fn is_synthesized_dir(&self, key: &Path) -> bool {
        self.spans_mounts(key) || (key.as_os_str().is_empty() && self.mounts.is_empty())
    }

    /// Refuse a mutation aimed at a directory the mount table synthesized.
    ///
    /// Checked *before* the backend is consulted, because whether the path is a
    /// directory is knowable from the mount table alone — and a backend asked first
    /// often answers `Ok`, which is how a mount ends up detached: `rmdir` succeeds
    /// on a directory the mount table still populates, `unlink` deletes a mount's
    /// parent, `open` creates a file where a mount has to be.
    fn guard_synthesized(&self, key: &Path, refusal: CortexError) -> Result<()> {
        if self.is_synthesized_dir(key) {
            Err(refusal)
        } else {
            Ok(())
        }
    }

    /// The error for a *create* at a path no mount claims.
    ///
    /// `ReadOnly` when the parent is a synthesized directory. This mirrors how a
    /// kernel orders the checks: a create resolves only the *parent*, that parent
    /// exists and was just listed, and the step that fails is the write — so
    /// `EROFS`, not the `ENOENT` that would claim a directory the caller can see
    /// is absent. Removals resolve the *target* instead, so they keep `NotFound`.
    ///
    /// `EROFS` and not `ENOSYS`: a FUSE kernel handed `ENOSYS` reads it as "this
    /// filesystem cannot do that at all" and disables the operation for the whole
    /// mount, taking every writable mount down with it. `EROFS` is per request.
    fn refusal_for_create(&self, key: &Path) -> CortexError {
        match key.parent() {
            Some(parent) if self.is_synthesized_dir(parent) => CortexError::ReadOnly,
            // The parent is not there either, so a component of the path really is
            // missing — the case POSIX answers with `ENOENT`.
            _ => CortexError::NotFound,
        }
    }

    /// The metadata a directory that exists only in the mount table reports.
    fn synthesized_dir(&self) -> Stat {
        Stat {
            mtime: Some(self.born),
            ctime: Some(self.born),
            created: Some(self.born),
            ..Stat::new(DirentKind::Dir, 0)
        }
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonicalize a request into a root-relative path of `Normal` components
/// only. `.` and a leading root are dropped and `..` pops the previous
/// component; any `..` that would escape the workspace root, and OS prefixes,
/// are rejected.
/// Normalize a path for use as a mount key, additionally refusing components that
/// are not valid UTF-8.
///
/// Only *mount* paths carry this restriction, not request paths: a request may
/// legitimately name a non-UTF-8 file inside a passthrough volume. But a mount
/// point's own name gets *listed* — reported through [`Dirent::name`], which is a
/// `String` — so a non-UTF-8 component could only be shown lossily, and a lossy
/// name does not round-trip. That would produce an entry visible in a listing
/// whose `lookup` then fails.
fn mount_key(path: &Path) -> Result<PathBuf> {
    let key = normalize(path)?;
    if key
        .components()
        .any(|component| component.as_os_str().to_str().is_none())
    {
        return Err(CortexError::InvalidName);
    }
    Ok(key)
}

fn normalize(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(CortexError::InvalidName);
                }
            }
            Component::Prefix(_) => return Err(CortexError::InvalidName),
        }
    }
    Ok(out)
}

#[async_trait]
impl Mountable for Workspace {
    type Handle = Box<dyn FileHandle>;

    async fn stat(&self, path: &Path) -> Result<Stat> {
        let key = normalize(path)?;
        // Collapsing "no mount claims this" and "a mount claimed it and answered
        // NotFound" into one `Err(NotFound)` is what makes the arm below cover
        // both. Only checking whether routing failed would miss the second, which
        // is the case a root backend produces for every path it does not know.
        let answered = match self.route(&key) {
            Some((backend, sub)) => backend.stat(&sub).await,
            None => Err(CortexError::NotFound),
        };
        match answered {
            Err(CortexError::NotFound) if self.is_synthesized_dir(&key) => {
                Ok(self.synthesized_dir())
            }
            // Anything else is the backend's answer, including other errors: a
            // `PermissionDenied` must not become an empty directory.
            other => other,
        }
    }

    async fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        let key = normalize(path)?;
        let children = self.mount_children(&key);

        // Same collapse as `stat`: no route and a routed `NotFound` are the same
        // question. A directory that exists only in the mount table simply has no
        // backend entries to start from.
        let answered = match self.route(&key) {
            Some((backend, sub)) => backend.list(&sub).await,
            None => Err(CortexError::NotFound),
        };
        let entries = match answered {
            Ok(entries) => entries,
            Err(CortexError::NotFound) if self.is_synthesized_dir(&key) => Vec::new(),
            other => return other,
        };

        // Split the backend's entries by whether the mount table also names them,
        // so each name is emitted exactly once.
        let (mut named_by_mounts, backend_only): (Vec<_>, Vec<_>) = entries
            .into_iter()
            .partition(|entry| children.contains_key(&entry.name));
        let mut claimed: BTreeMap<String, Dirent> = named_by_mounts
            .drain(..)
            .map(|entry| (entry.name.clone(), entry))
            .collect();

        // Mount-derived names go first, in sorted order, so their positions depend
        // only on the mount table. `readdir`'s cursor is a position in this list and
        // the kernel resumes by quoting one, so putting them last would let a single
        // file appearing in the backend shift every mount point — dropping a whole
        // mount out of an in-progress listing.
        let mut out = Vec::with_capacity(children.len() + backend_only.len());
        for (name, mounted_here) in children {
            let backend_entry = claimed.remove(&name);
            out.push(match (mounted_here, backend_entry) {
                // A mount at this exact path shadows whatever the backend had,
                // the way any mount hides the directory it is mounted over. No
                // stat: the kernel will `lookup` for metadata, and that lookup
                // routes to the mount, so the two answers cannot disagree.
                (true, _) => Dirent::new(name, DirentKind::Dir),
                // Merely on the way to a deeper mount, and the backend has it —
                // keep its entry, which carries metadata a synthesized one lacks.
                (false, Some(entry)) => entry,
                (false, None) => Dirent::new(name, DirentKind::Dir),
            });
        }
        out.extend(backend_only);
        Ok(out)
    }

    async fn mkdir(&self, path: &Path) -> Result<()> {
        let key = normalize(path)?;
        // Already a directory, so this is `EEXIST` — not the `Ok(())` the backends
        // give for an existing directory. Recorded as a deliberate divergence: a
        // workspace answers for the whole namespace, where POSIX wants `EEXIST`.
        self.guard_synthesized(&key, CortexError::AlreadyExists)?;
        let r = match self.route(&key) {
            Some((backend, sub)) => backend.mkdir(&sub).await,
            None => Err(self.refusal_for_create(&key)),
        };
        // Symmetric with `rmdir`'s `Removed`: a created directory is a change too.
        // `Created` may name a directory (see the module docs on directory events).
        if r.is_ok() {
            self.fire(FsEvent::Created(&key.to_string_lossy()));
        }
        r
    }

    async fn unlink(&self, path: &Path) -> Result<()> {
        let key = normalize(path)?;
        self.guard_synthesized(&key, CortexError::IsADirectory)?;
        let r = match self.route(&key) {
            Some((backend, sub)) => backend.unlink(&sub).await,
            None => Err(CortexError::NotFound),
        };
        if r.is_ok() {
            self.fire(FsEvent::Removed(&key.to_string_lossy()));
        }
        r
    }

    async fn rmdir(&self, path: &Path) -> Result<()> {
        let key = normalize(path)?;
        // Not empty: it holds the mount points below it, and those are not the
        // caller's to remove through the filesystem.
        self.guard_synthesized(&key, CortexError::NotEmpty)?;
        let r = match self.route(&key) {
            Some((backend, sub)) => backend.rmdir(&sub).await,
            None => Err(CortexError::NotFound),
        };
        if r.is_ok() {
            self.fire(FsEvent::Removed(&key.to_string_lossy()));
        }
        r
    }

    async fn open(&self, path: &Path, options: OpenOptions) -> Result<(Self::Handle, Stat)> {
        let key = normalize(path)?;
        self.guard_synthesized(&key, CortexError::IsADirectory)?;
        // Whether this open can mutate — decides if a write hook is armed. The
        // create-vs-modify split is by pre-existence at open, checked only when a
        // hook is actually attached. Uses the same predicate as a read-only
        // backend's refusal, so `append` (which grants OS-level write on a
        // passthrough mount even without `write`) still arms the hook. The refusal
        // below only checks `create` because `create_new` implies `create`
        // upstream — a narrower question than "can this mutate".
        let writing = options.intends_write();
        let pre_existed = if writing && self.hook.is_some() {
            match self.route(&key) {
                Some((b, s)) => b.stat(&s).await.is_ok(),
                None => false,
            }
        } else {
            false
        };
        let (handle, stat) = match self.route(&key) {
            Some((backend, sub)) => backend.open(&sub, options).await?,
            None if options.create => {
                return Err(self.refusal_for_create(&key));
            }
            None => return Err(CortexError::NotFound),
        };
        match (&self.hook, writing) {
            (Some(hook), true) => {
                let hooked = HookedHandle {
                    inner: handle,
                    hook: hook.clone(),
                    path: key.to_string_lossy().into_owned(),
                    pre_existed,
                    fired: AtomicBool::new(false),
                };
                Ok((Box::new(hooked), stat))
            }
            _ => Ok((handle, stat)),
        }
    }

    async fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let (from_key, to_key) = (normalize(from)?, normalize(to)?);

        // Neither end may be something the mount table owns. Checked before the
        // backends are consulted, because a refusal has to leave the tree untouched
        // and a backend asked about its own root would answer something arbitrary.
        if self.is_mount_table_owned(&from_key) || self.is_mount_table_owned(&to_key) {
            return Err(CortexError::ReadOnly);
        }

        let (Some(mount), Some(destination)) = (self.mount_for(&from_key), self.mount_for(&to_key))
        else {
            // A move resolves its source before anything else, so a source nothing
            // claims is simply absent. The destination is a *create*, which is why
            // it gets the create refusal instead.
            return if self.mount_for(&from_key).is_none() {
                Err(CortexError::NotFound)
            } else {
                Err(self.refusal_for_create(&to_key))
            };
        };

        // Two backends cannot hand an object to each other, so this is `EXDEV` — and
        // `mv` reads that as "copy, then delete", which is the right recovery across
        // a store boundary. Doing the copy here instead could not be made atomic on
        // partial failure, and `mv` already knows how to deal with that.
        if mount != destination {
            return Err(CortexError::CrossDevice);
        }

        let (backend, from_sub) = self
            .route(&from_key)
            .expect("a mount was just found for this key");
        let to_sub = to_key
            .strip_prefix(mount)
            .expect("both paths share this mount");
        let to_existed = self.hook.is_some() && backend.stat(to_sub).await.is_ok();
        let r = backend.rename(&from_sub, to_sub).await;
        if r.is_ok() {
            let from_s = from_key.to_string_lossy();
            let to_s = to_key.to_string_lossy();
            self.fire(FsEvent::Removed(&from_s));
            self.fire(if to_existed {
                FsEvent::Modified(&to_s)
            } else {
                FsEvent::Created(&to_s)
            });
        }
        r
    }
}

/// A serializable description of a whole [`Workspace`]'s *namespace*: the ordered
/// set of `(mount_path, volume)` pairs.
///
/// This is the artifact a caller reuses across a process boundary. A live
/// `Workspace` holds open clients and runtimes that cannot cross to another
/// process, so the thing shared between (say) a WebDAV server and a sandbox's VM
/// helper is this spec — each side calls [`Workspace::from_spec`] to build its
/// own live tree with an identical namespace.
///
/// The namespace is all that crosses. A live `Workspace`'s two instance-local
/// fields stay behind: its [`hook`](Workspace::with_hook), which is host
/// behaviour no serialization can carry (and which the far side has nothing to
/// react with), and its `born` timestamp. Each side attaches its own hook after
/// [`from_spec`](Workspace::from_spec) — see that method for the reasoning.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceSpec {
    /// `(mount_path, volume)` in insertion order. A mount at the empty path is
    /// the root; deeper mounts shadow it (longest-prefix, see [`Workspace`]).
    pub mounts: Vec<(String, crate::VolumeSpec)>,
}

impl WorkspaceSpec {
    /// Builder-style: append a mount of `volume` at `path`.
    pub fn mount(mut self, path: impl Into<String>, volume: crate::VolumeSpec) -> Self {
        self.mounts.push((path.into(), volume));
        self
    }
}

/// A [`FileHandle`] that fires a workspace [`FsHook`] once, on the first
/// *successful* `flush`/`commit`, with the create-vs-modify distinction fixed at
/// open. Wraps the backend handle so positioned I/O passes straight through; only
/// the write-out is observed.
struct HookedHandle {
    inner: Box<dyn FileHandle>,
    hook: Arc<dyn FsHook>,
    path: String,
    pre_existed: bool,
    fired: AtomicBool,
}

impl HookedHandle {
    fn fire_once(&self) {
        if !self.fired.swap(true, Ordering::Relaxed) {
            self.hook.on_change(if self.pre_existed {
                FsEvent::Modified(&self.path)
            } else {
                FsEvent::Created(&self.path)
            });
        }
    }
}

#[async_trait]
impl FileExt for HookedHandle {
    async fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        self.inner.read_at(buf, offset).await
    }
    async fn write_at(&self, buf: &[u8], offset: u64) -> std::io::Result<usize> {
        self.inner.write_at(buf, offset).await
    }
}

#[async_trait]
impl FileHandle for HookedHandle {
    async fn truncate(&self, size: u64) -> Result<()> {
        self.inner.truncate(size).await
    }
    async fn flush(&self) -> Result<()> {
        let r = self.inner.flush().await;
        // Fires here because the intended host is a flush-is-finalize frontend (a
        // WebDAV `PUT` flushes once at the end); see the module docs on why a
        // mid-write-flush mount is not an intended hook host. Only a write that
        // landed is a change to announce — firing on a failed flush would have a
        // consumer index bytes the backend never persisted. Matches the `is_ok`
        // gate on `unlink`/`rmdir`/`rename`.
        if r.is_ok() {
            self.fire_once();
        }
        r
    }
    async fn commit(&self) -> Result<()> {
        let r = self.inner.commit().await;
        if r.is_ok() {
            self.fire_once();
        }
        r
    }
}

// Tests live beside this file rather than inside it: they had grown longer than
// the implementation, so a reader opening it had to scroll past them to find the
// code. They are still a child module, so private items stay reachable.
#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
