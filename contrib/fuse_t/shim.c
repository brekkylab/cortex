/* Translation between libfuse-t's lowlevel callbacks and cortex's flat vtable.
 *
 * Handing the session loop back to libfuse-t is the whole reason this exists —
 * see `src/mountable/adapter/fuse_t.rs`. No logic beyond marshalling; every
 * decision belongs to the shared layer on the Rust side.
 */

#define FUSE_USE_VERSION 26

#include "shim.h"

#include <errno.h>
#include <fuse_lowlevel.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statvfs.h>

/* Mirrors the Rust side's TTL. Spelled here rather than plumbed through the
 * vtable, because libfuse wants it as a double.
 *
 * Two copies of one number, with nothing checking they agree: changing
 * `posix::TTL` and not this leaves the FUSE-T mount caching for a different
 * window than the other bindings, silently. Plumb it through
 * `cortex_fuse_t_ops` if it ever needs to be configurable. */
#define CORTEX_TTL 1.0

struct session {
    struct fuse_chan *ch;
    struct fuse_session *se;
    char *mountpoint;
    void *fs;
    struct cortex_fuse_t_ops ops;
};

static struct session *ctx(fuse_req_t req) {
    return (struct session *)fuse_req_userdata(req);
}

static void widen(const struct cortex_stat *in, struct stat *out) {
    memset(out, 0, sizeof *out);
    out->st_ino = in->ino;
    out->st_size = (off_t)in->size;
    out->st_blocks = (blkcnt_t)in->blocks;
    out->st_mode = (mode_t)in->mode;
    out->st_nlink = (nlink_t)in->nlink;
    out->st_blksize = (blksize_t)in->blksize;
    out->st_mtimespec.tv_sec = (time_t)in->mtime;
    out->st_mtimespec.tv_nsec = (long)in->mtime_nsec;
    out->st_atimespec.tv_sec = (time_t)in->atime;
    out->st_atimespec.tv_nsec = (long)in->atime_nsec;
    out->st_ctimespec.tv_sec = (time_t)in->ctime;
    out->st_ctimespec.tv_nsec = (long)in->ctime_nsec;
    /* Served from this process, so the mounting user owns what it sees. */
    out->st_uid = getuid();
    out->st_gid = getgid();
}

/* Replies if `err` is a negative errno, returning 1 so callers can bail. */
static int replied_error(fuse_req_t req, int err) {
    if (err != 0) {
        fuse_reply_err(req, -err);
        return 1;
    }
    return 0;
}

static void ll_lookup(fuse_req_t req, fuse_ino_t parent, const char *name) {
    struct session *s = ctx(req);
    uint64_t ino = 0;
    struct cortex_stat cs;
    if (replied_error(req, s->ops.lookup(s->fs, parent, name, &ino, &cs))) return;

    struct fuse_entry_param e;
    memset(&e, 0, sizeof e);
    e.ino = (fuse_ino_t)ino;
    /* Inode numbers are never reused, so no generation is needed. */
    e.generation = 0;
    e.attr_timeout = CORTEX_TTL;
    e.entry_timeout = CORTEX_TTL;
    widen(&cs, &e.attr);
    fuse_reply_entry(req, &e);
}

static void ll_forget(fuse_req_t req, fuse_ino_t ino, unsigned long nlookup) {
    struct session *s = ctx(req);
    s->ops.forget(s->fs, ino, nlookup);
    fuse_reply_none(req);
}

static void ll_getattr(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi) {
    (void)fi;
    struct session *s = ctx(req);
    struct cortex_stat cs;
    if (replied_error(req, s->ops.getattr(s->fs, ino, &cs))) return;
    struct stat st;
    widen(&cs, &st);
    fuse_reply_attr(req, &st, CORTEX_TTL);
}

static void ll_setattr(fuse_req_t req, fuse_ino_t ino, struct stat *attr, int to_set,
                       struct fuse_file_info *fi) {
    struct session *s = ctx(req);
    int has_size = (to_set & FUSE_SET_ATTR_SIZE) != 0;
    uint64_t size = has_size ? (uint64_t)attr->st_size : 0;
    /* Mode, ownership, and timestamps are dropped; see `setattr_inode`. */
    struct cortex_stat cs;
    int err = s->ops.setattr(s->fs, ino, fi ? fi->fh : 0, fi ? 1 : 0, size, has_size, &cs);
    if (replied_error(req, err)) return;
    struct stat st;
    widen(&cs, &st);
    fuse_reply_attr(req, &st, CORTEX_TTL);
}

static void ll_open(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi) {
    struct session *s = ctx(req);
    uint64_t fh = 0;
    if (replied_error(req, s->ops.open(s->fs, ino, fi->flags, &fh))) return;
    fi->fh = fh;
    fuse_reply_open(req, fi);
}

static void ll_create(fuse_req_t req, fuse_ino_t parent, const char *name, mode_t mode,
                      struct fuse_file_info *fi) {
    (void)mode;
    struct session *s = ctx(req);
    uint64_t ino = 0, fh = 0;
    struct cortex_stat cs;
    int err = s->ops.create(s->fs, parent, name, fi->flags, &ino, &fh, &cs);
    if (replied_error(req, err)) return;
    fi->fh = fh;

    struct fuse_entry_param e;
    memset(&e, 0, sizeof e);
    e.ino = (fuse_ino_t)ino;
    e.generation = 0;
    e.attr_timeout = CORTEX_TTL;
    e.entry_timeout = CORTEX_TTL;
    widen(&cs, &e.attr);
    fuse_reply_create(req, &e, fi);
}

static void ll_read(fuse_req_t req, fuse_ino_t ino, size_t size, off_t off,
                    struct fuse_file_info *fi) {
    (void)ino;
    struct session *s = ctx(req);
    char *buf = malloc(size ? size : 1);
    if (!buf) {
        fuse_reply_err(req, ENOMEM);
        return;
    }
    long n = s->ops.read(s->fs, fi->fh, (uint64_t)off, size, buf);
    if (n < 0) fuse_reply_err(req, (int)-n);
    else fuse_reply_buf(req, buf, (size_t)n);
    free(buf);
}

static void ll_write(fuse_req_t req, fuse_ino_t ino, const char *buf, size_t size,
                     off_t off, struct fuse_file_info *fi) {
    (void)ino;
    struct session *s = ctx(req);
    long n = s->ops.write(s->fs, fi->fh, (uint64_t)off, size, buf);
    if (n < 0) fuse_reply_err(req, (int)-n);
    else fuse_reply_write(req, (size_t)n);
}

static void ll_flush(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi) {
    (void)ino;
    struct session *s = ctx(req);
    /* Not `release`: arrives on every `close()`, so it must not finalize. */
    fuse_reply_err(req, -s->ops.flush(s->fs, fi->fh));
}

static void ll_fsync(fuse_req_t req, fuse_ino_t ino, int datasync,
                     struct fuse_file_info *fi) {
    (void)ino;
    (void)datasync;
    struct session *s = ctx(req);
    fuse_reply_err(req, -s->ops.flush(s->fs, fi->fh));
}

static void ll_release(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi) {
    (void)ino;
    struct session *s = ctx(req);
    fuse_reply_err(req, -s->ops.release(s->fs, fi->fh));
}

static void ll_mkdir(fuse_req_t req, fuse_ino_t parent, const char *name, mode_t mode) {
    (void)mode;
    struct session *s = ctx(req);
    uint64_t ino = 0;
    struct cortex_stat cs;
    if (replied_error(req, s->ops.mkdir(s->fs, parent, name, &ino, &cs))) return;

    struct fuse_entry_param e;
    memset(&e, 0, sizeof e);
    e.ino = (fuse_ino_t)ino;
    e.generation = 0;
    e.attr_timeout = CORTEX_TTL;
    e.entry_timeout = CORTEX_TTL;
    widen(&cs, &e.attr);
    fuse_reply_entry(req, &e);
}

static void ll_unlink(fuse_req_t req, fuse_ino_t parent, const char *name) {
    struct session *s = ctx(req);
    fuse_reply_err(req, -s->ops.unlink(s->fs, parent, name));
}

static void ll_rmdir(fuse_req_t req, fuse_ino_t parent, const char *name) {
    struct session *s = ctx(req);
    fuse_reply_err(req, -s->ops.rmdir(s->fs, parent, name));
}

static void ll_rename(fuse_req_t req, fuse_ino_t parent, const char *name,
                      fuse_ino_t newparent, const char *newname) {
    struct session *s = ctx(req);
    fuse_reply_err(req, -s->ops.rename(s->fs, parent, name, newparent, newname));
}

/* Accumulates entries into libfuse's buffer — the one part of `readdir` the
 * caller cannot do itself, since `fuse_add_direntry` needs the request. */
struct dirbuf {
    fuse_req_t req;
    char *buf;
    size_t size;
    size_t used;
};

static int emit_dirent(void *sink, uint64_t ino, const char *name, uint32_t mode,
                       uint64_t next_offset) {
    struct dirbuf *d = sink;
    size_t need = fuse_add_direntry(d->req, NULL, 0, name, NULL, 0);
    if (d->used + need > d->size) return 1; /* full: stop */
    struct stat st;
    memset(&st, 0, sizeof st);
    st.st_ino = ino;
    st.st_mode = (mode_t)mode;
    fuse_add_direntry(d->req, d->buf + d->used, d->size - d->used, name, &st,
                      (off_t)next_offset);
    d->used += need;
    return 0;
}

static void ll_readdir(fuse_req_t req, fuse_ino_t ino, size_t size, off_t off,
                       struct fuse_file_info *fi) {
    (void)fi;
    struct session *s = ctx(req);
    struct dirbuf d = {req, calloc(1, size ? size : 1), size, 0};
    if (!d.buf) {
        fuse_reply_err(req, ENOMEM);
        return;
    }
    int err = s->ops.readdir(s->fs, ino, (uint64_t)off, &d, emit_dirent);
    if (err != 0) fuse_reply_err(req, -err);
    else fuse_reply_buf(req, d.buf, d.used);
    free(d.buf);
}

static void ll_statfs(fuse_req_t req, fuse_ino_t ino) {
    (void)ino;
    struct session *s = ctx(req);
    struct statvfs v;
    memset(&v, 0, sizeof v);
    v.f_bsize = s->ops.block_size;
    /* glibc's `statvfs` consumers divide by `f_frsize`; zero is a div-by-zero. */
    v.f_frsize = s->ops.block_size;
    v.f_namemax = s->ops.name_max;
    v.f_blocks = s->ops.total_blocks;
    v.f_bfree = s->ops.total_blocks;
    v.f_bavail = s->ops.total_blocks;
    v.f_files = s->ops.total_inodes;
    v.f_ffree = s->ops.total_inodes;
    v.f_favail = s->ops.total_inodes;
    fuse_reply_statfs(req, &v);
}

/* Only what cortex implements. libfuse answers the rest with ENOSYS, matching
 * the other bindings: symlinks, hard links, xattrs, locks. */
static const struct fuse_lowlevel_ops LL_OPS = {
    .lookup = ll_lookup,
    .forget = ll_forget,
    .getattr = ll_getattr,
    .setattr = ll_setattr,
    .mkdir = ll_mkdir,
    .unlink = ll_unlink,
    .rmdir = ll_rmdir,
    .rename = ll_rename,
    .open = ll_open,
    .read = ll_read,
    .write = ll_write,
    .flush = ll_flush,
    .release = ll_release,
    .fsync = ll_fsync,
    .readdir = ll_readdir,
    .statfs = ll_statfs,
    .create = ll_create,
};

void *cortex_fuse_t_mount(const char *mountpoint, const char *fsname, void *fs,
                          const struct cortex_fuse_t_ops *ops) {
    struct session *s = calloc(1, sizeof *s);
    if (!s) return NULL;
    s->fs = fs;
    s->ops = *ops;
    s->mountpoint = strdup(mountpoint);
    if (!s->mountpoint) goto fail;

    struct fuse_args args = FUSE_ARGS_INIT(0, NULL);
    if (fuse_opt_add_arg(&args, "cortex") != 0) goto fail_args;
    if (fuse_opt_add_arg(&args, "-o") != 0) goto fail_args;
    {
        char opt[256];
        snprintf(opt, sizeof opt, "fsname=%s", fsname);
        if (fuse_opt_add_arg(&args, opt) != 0) goto fail_args;
    }

    s->ch = fuse_mount(s->mountpoint, &args);
    if (!s->ch) goto fail_args;
    s->se = fuse_lowlevel_new(&args, &LL_OPS, sizeof LL_OPS, s);
    if (!s->se) {
        fuse_unmount(s->mountpoint, s->ch);
        goto fail_args;
    }
    fuse_session_add_chan(s->se, s->ch);
    fuse_opt_free_args(&args);
    return s;

fail_args:
    fuse_opt_free_args(&args);
fail:
    free(s->mountpoint);
    free(s);
    return NULL;
}

int cortex_fuse_t_loop(void *session) {
    struct session *s = session;
    return fuse_session_loop(s->se);
}

void cortex_fuse_t_unmount(void *session) {
    struct session *s = session;
    /* Exit flag first: the loop checks it between requests, and the unmount
     * below closes the channel so an in-flight read returns. Together they are
     * what let the loop come back instead of deadlocking the caller's join. */
    if (s->se) fuse_session_exit(s->se);
    if (s->ch) {
        /* This is what breaks the blocking loop. */
        fuse_session_remove_chan(s->ch);
        fuse_unmount(s->mountpoint, s->ch);
        s->ch = NULL;
    }
}

void cortex_fuse_t_destroy(void *session) {
    struct session *s = session;
    if (!s) return;
    cortex_fuse_t_unmount(s);
    if (s->se) fuse_session_destroy(s->se);
    free(s->mountpoint);
    free(s);
}
