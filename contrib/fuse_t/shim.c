/* Translation between libfuse-t's lowlevel callbacks and cortex's flat vtable.
 *
 * Exists so libfuse-t's own loop drives the session (see
 * `src/fs/mount/impl/fuse_t.rs`). Marshalling only; every decision is made on
 * the Rust side.
 */

#include "shim.h"

/* libfuse-t's interface, declared rather than taken from FUSE-T's headers, so that this
 * builds on a host without FUSE-T -- see `fuse_t.h`, and `check-abi.sh` for what keeps it
 * right. */
#include "fuse_t.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <unistd.h>

/* libfuse-t, opened at run time rather than linked.
 *
 * So a binary (or Python/Node extension) holding this shim starts or imports
 * without FUSE-T installed; most callers never mount. Linking would also need
 * an `LC_RPATH` in every final binary, which `rustc-link-arg` cannot give a
 * dependent, and without it a weak import is null anyway.
 *
 * Each function called is a `dlsym`-filled pointer here, with a macro so call
 * sites use libfuse's own names. `cortex_fuse_t_status` fills them once; call
 * nothing before it answers CORTEX_FUSE_T_OK. The list must be complete: a
 * missing entry is a link error, since the library is not on the link line. */
#include <dlfcn.h>
#include <limits.h>
#include <pthread.h>

#define CORTEX_FUSE_T_FNS(X)                                                  \
    X(fuse_add_direntry)                                                      \
    X(fuse_chan_fd)                                                           \
    X(fuse_lowlevel_new)                                                      \
    X(fuse_mount)                                                             \
    X(fuse_opt_add_arg)                                                       \
    X(fuse_opt_free_args)                                                     \
    X(fuse_reply_attr)                                                        \
    X(fuse_reply_buf)                                                         \
    X(fuse_reply_create)                                                      \
    X(fuse_reply_entry)                                                       \
    X(fuse_reply_err)                                                         \
    X(fuse_reply_none)                                                        \
    X(fuse_reply_open)                                                        \
    X(fuse_reply_statfs)                                                      \
    X(fuse_reply_write)                                                       \
    X(fuse_req_userdata)                                                      \
    X(fuse_session_add_chan)                                                  \
    X(fuse_session_destroy)                                                   \
    X(fuse_session_exit)                                                      \
    X(fuse_session_loop)                                                      \
    X(fuse_unmount)

#define CORTEX_FUSE_T_POINTER(name) static __typeof__(name) *cortex_p_##name;
CORTEX_FUSE_T_FNS(CORTEX_FUSE_T_POINTER)

/* The library's install path as `pkg-config` reported it at build time
 * (defined by `build.rs`). The bare name is tried first, so one the loader
 * finds itself (`DYLD_LIBRARY_PATH`, fallback paths) wins. */
#ifndef CORTEX_FUSE_T_LIBDIR
#define CORTEX_FUSE_T_LIBDIR "/usr/local/lib"
#endif

static int cortex_fuse_t_state = CORTEX_FUSE_T_MISSING;
static int cortex_fuse_t_api_version;
static char cortex_fuse_t_release_name[32];
static pthread_once_t cortex_fuse_t_once = PTHREAD_ONCE_INIT;

/* The release the file `symbol` is in says it is, by the name FUSE-T's installer gives it:
 * `libfuse-t.dylib` is a link to `libfuse-t-<release>.dylib`. Left empty when the name is
 * not of that form. The library itself carries no version: its `LC_ID_DYLIB` says 0.0.0,
 * and `fuse_version()` is libfuse's API, not FUSE-T's release. */
static void cortex_fuse_t_read_release(void *symbol) {
    static const char prefix[] = "libfuse-t-", suffix[] = ".dylib";
    const size_t pre = sizeof prefix - 1, suf = sizeof suffix - 1;
    Dl_info info;
    char real[PATH_MAX];
    if (!dladdr(symbol, &info) || !info.dli_fname || !realpath(info.dli_fname, real)) return;
    const char *base = strrchr(real, '/');
    base = base ? base + 1 : real;
    size_t n = strlen(base);
    if (n <= pre + suf || strncmp(base, prefix, pre) || strcmp(base + n - suf, suffix)) return;
    size_t len = n - pre - suf;
    if (len >= sizeof cortex_fuse_t_release_name) return;
    memcpy(cortex_fuse_t_release_name, base + pre, len);
    cortex_fuse_t_release_name[len] = 0;
}

/* `CORTEX_FUSE_T_UNCHECKED`, set to anything but empty or `0`: use a libfuse-t these
 * declarations were not checked against, at the risk of a crash. */
static int cortex_fuse_t_unchecked(void) {
    const char *v = getenv("CORTEX_FUSE_T_UNCHECKED");
    return v && *v && strcmp(v, "0") != 0;
}

static void cortex_fuse_t_load(void) {
    void *lib = dlopen("libfuse-t.dylib", RTLD_NOW | RTLD_LOCAL);
    if (!lib)
        lib = dlopen(CORTEX_FUSE_T_LIBDIR "/libfuse-t.dylib", RTLD_NOW | RTLD_LOCAL);
    if (!lib)
        return;

    /* Which libfuse-t this is, before anything is called that shares a layout with it:
     * `fuse_version` takes nothing and returns an int, so it is safe to call whatever
     * the library is. */
    int (*version)(void) = (int (*)(void))dlsym(lib, "fuse_version");
    if (!version)
        return;
    cortex_fuse_t_api_version = version();
    cortex_fuse_t_read_release((void *)version);
    if (!cortex_fuse_t_unchecked()) {
        if (cortex_fuse_t_api_version < CORTEX_FUSE_T_API_MIN ||
            cortex_fuse_t_api_version > CORTEX_FUSE_T_API_MAX) {
            cortex_fuse_t_state = CORTEX_FUSE_T_OTHER_API;
            return;
        }
        /* A release the name does not say -- no name of that form, or one that does not
         * start with a number -- is let through on its API alone: refusing it would
         * refuse an install that is only named differently. */
        const char *r = cortex_fuse_t_release_name;
        if (r[0] >= '0' && r[0] <= '9' && atoi(r) != CORTEX_FUSE_T_MAJOR) {
            cortex_fuse_t_state = CORTEX_FUSE_T_OTHER_MAJOR;
            return;
        }
    }

    /* Never closed: every mount this process makes calls through these. */
#define CORTEX_FUSE_T_RESOLVE(name)                                           \
    if (!(cortex_p_##name = (__typeof__(name) *)dlsym(lib, #name)))           \
        return;
    CORTEX_FUSE_T_FNS(CORTEX_FUSE_T_RESOLVE)
    cortex_fuse_t_state = CORTEX_FUSE_T_OK;
}

int cortex_fuse_t_status(void) {
    pthread_once(&cortex_fuse_t_once, cortex_fuse_t_load);
    return cortex_fuse_t_state;
}

int cortex_fuse_t_api(void) {
    pthread_once(&cortex_fuse_t_once, cortex_fuse_t_load);
    return cortex_fuse_t_api_version;
}

const char *cortex_fuse_t_release(void) {
    pthread_once(&cortex_fuse_t_once, cortex_fuse_t_load);
    return cortex_fuse_t_release_name;
}

const char *cortex_fuse_t_checked(void) {
    return CORTEX_FUSE_T_CHECKED;
}

#define fuse_add_direntry cortex_p_fuse_add_direntry
#define fuse_chan_fd cortex_p_fuse_chan_fd
#define fuse_lowlevel_new cortex_p_fuse_lowlevel_new
#define fuse_mount cortex_p_fuse_mount
#define fuse_opt_add_arg cortex_p_fuse_opt_add_arg
#define fuse_opt_free_args cortex_p_fuse_opt_free_args
#define fuse_reply_attr cortex_p_fuse_reply_attr
#define fuse_reply_buf cortex_p_fuse_reply_buf
#define fuse_reply_create cortex_p_fuse_reply_create
#define fuse_reply_entry cortex_p_fuse_reply_entry
#define fuse_reply_err cortex_p_fuse_reply_err
#define fuse_reply_none cortex_p_fuse_reply_none
#define fuse_reply_open cortex_p_fuse_reply_open
#define fuse_reply_statfs cortex_p_fuse_reply_statfs
#define fuse_reply_write cortex_p_fuse_reply_write
#define fuse_req_userdata cortex_p_fuse_req_userdata
#define fuse_session_add_chan cortex_p_fuse_session_add_chan
#define fuse_session_destroy cortex_p_fuse_session_destroy
#define fuse_session_exit cortex_p_fuse_session_exit
#define fuse_session_loop cortex_p_fuse_session_loop
#define fuse_unmount cortex_p_fuse_unmount

/* Must equal `posix::TTL`; nothing checks it, and a mismatch silently changes
 * this mount's cache window. Duplicated because libfuse wants a double; plumb
 * it through `cortex_fuse_t_ops` if it must become configurable. */
#define CORTEX_TTL 1.0

struct session {
    struct fuse_chan *ch;
    struct fuse_session *se;
    char *mountpoint;
    void *fs;
    /* Set once told to stop, so `cortex_fuse_t_destroy` can stop
     * unconditionally. */
    int stopped;
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
    /* Only size is forwarded; mode, ownership and timestamps are dropped. */
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

/* Accumulates entries into libfuse's buffer; done here because
 * `fuse_add_direntry` needs the request. */
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

/* Only what cortex implements. libfuse answers the rest (symlinks, hard links,
 * xattrs, locks) with ENOSYS. */
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

void *cortex_fuse_t_mount(const char *mountpoint, const char *fsname,
                          const char *backend, void *fs,
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
        /* One comma-separated `-o`. Both values are ours and short, and
         * `snprintf` bounds the buffer, so truncation is not checked. */
        char opt[256];
        int n = snprintf(opt, sizeof opt, "fsname=%s", fsname);
        if (n < 0) goto fail_args;
        if (backend && (size_t)n < sizeof opt) {
            /* Omitted when NULL so a user's `fuse-t.ini` choice applies. */
            snprintf(opt + n, sizeof opt - (size_t)n, ",backend=%s", backend);
        }
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

void cortex_fuse_t_stop(void *session) {
    struct session *s = session;
    if (!s || s->stopped) return;
    s->stopped = 1;

    /* The loop checks the exit flag only between requests; it is blocked in
     * `recvfrom`, so the shutdown below is what wakes it. */
    if (s->se) fuse_session_exit(s->se);
    if (!s->ch) return;

    /* `shutdown`, not `close`: it wakes `recvfrom` with EOF but keeps the
     * descriptor, so `fuse_session_destroy` closes it exactly once; a second
     * close could hit a number reused by another thread.
     *
     * The channel stays attached until `fuse_session_destroy`, when the loop is
     * done. Detaching here would NULL `ch->se` under the serving thread (an
     * in-flight reply asserts "se != NULL" in `fuse_kern_chan_send`) and NULL
     * `se->ch`, so `fuse_chan_destroy` would never run and the channel leaks. */
    int fd = fuse_chan_fd(s->ch);
    if (fd >= 0) shutdown(fd, SHUT_RDWR);
}

void cortex_fuse_t_destroy(void *session) {
    struct session *s = session;
    if (!s) return;
    cortex_fuse_t_stop(s);
    /* Takes the channel with it, and with it the one close of its descriptor. */
    if (s->se) fuse_session_destroy(s->se);
    free(s->mountpoint);
    free(s);
}
