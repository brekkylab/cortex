/* The boundary between virtx and libfuse-t.
 *
 * `fuse_lowlevel_ops` (~50 function pointers, `__APPLE__`-conditional members),
 * `fuse_file_info` (bitfields) and `fuse_entry_param` (embeds a host
 * `struct stat`) stay in C: a wrong layout in Rust is silent memory corruption.
 * They are declared in `fuse_t.h` and checked against FUSE-T's own headers by
 * `check-abi.sh`. Rust sees only the flat types below.
 *
 * Rust supplies `virtx_fuse_t_ops`. Operations return 0 or a negative errno,
 * as libfuse does.
 */

#ifndef VIRTX_FUSE_T_SHIM_H
#define VIRTX_FUSE_T_SHIM_H

#include <stddef.h>
#include <stdint.h>

/* A `struct stat` reduced to what virtx reports. Fixed-width with an explicit
 * pad so the Rust mirror is trivially correct; C widens it. */
struct virtx_stat {
    uint64_t ino;
    uint64_t size;
    uint64_t blocks;
    uint32_t mode; /* S_IF* | permission bits */
    uint32_t nlink;
    uint32_t blksize;
    uint32_t _pad;
    int64_t mtime;
    int64_t mtime_nsec;
    int64_t atime;
    int64_t atime_nsec;
    int64_t ctime;
    int64_t ctime_nsec;
};

/* Emits one directory entry: 0 while there is room, 1 once the kernel's buffer
 * is full. */
typedef int (*virtx_dirent_sink)(void *sink, uint64_t ino, const char *name,
                                  uint32_t mode, uint64_t next_offset);

/* Implemented in Rust. `fs` is the opaque filesystem pointer handed to
 * `virtx_fuse_t_mount`. */
struct virtx_fuse_t_ops {
    int (*lookup)(void *fs, uint64_t parent, const char *name, uint64_t *ino,
                  struct virtx_stat *out);
    int (*getattr)(void *fs, uint64_t ino, struct virtx_stat *out);
    /* `has_size` distinguishes "resize to 0" from "size not being set". */
    int (*setattr)(void *fs, uint64_t ino, uint64_t fh, int has_fh, uint64_t size,
                   int has_size, struct virtx_stat *out);
    int (*open)(void *fs, uint64_t ino, int flags, uint64_t *fh);
    int (*create)(void *fs, uint64_t parent, const char *name, int flags,
                  uint64_t *ino, uint64_t *fh, struct virtx_stat *out);
    /* Byte counts on success, negative errno on failure. */
    long (*read)(void *fs, uint64_t fh, uint64_t offset, uint64_t size, char *buf);
    long (*write)(void *fs, uint64_t fh, uint64_t offset, uint64_t size,
                  const char *buf);
    int (*flush)(void *fs, uint64_t fh);
    int (*release)(void *fs, uint64_t fh);
    int (*mkdir)(void *fs, uint64_t parent, const char *name, uint64_t *ino,
                 struct virtx_stat *out);
    int (*unlink)(void *fs, uint64_t parent, const char *name);
    int (*rmdir)(void *fs, uint64_t parent, const char *name);
    /* No flags: libfuse-t's `rename` has none, so `RENAME_NOREPLACE`/
     * `RENAME_EXCHANGE` never reach Rust. */
    int (*rename)(void *fs, uint64_t parent, const char *name, uint64_t newparent,
                  const char *newname);
    int (*readdir)(void *fs, uint64_t ino, uint64_t offset, void *sink,
                   virtx_dirent_sink emit);
    void (*forget)(void *fs, uint64_t ino, uint64_t nlookup);
    /* Reported verbatim in the `statfs` reply. */
    uint64_t total_blocks;
    uint64_t total_inodes;
    uint32_t block_size;
    uint32_t name_max;
};

/* What `virtx_fuse_t_status` answers. */
#define VIRTX_FUSE_T_MISSING 0 /* no libfuse-t, or one without a function the shim calls */
#define VIRTX_FUSE_T_OK 1
#define VIRTX_FUSE_T_OTHER_API 2 /* a libfuse API other than 2.x: `virtx_fuse_t_api` */
#define VIRTX_FUSE_T_OTHER_MAJOR 3 /* a FUSE-T release of another major version: `virtx_fuse_t_release` */

/* Open libfuse-t once per process, check the shim's declarations fit it, and resolve every
 * function the shim calls. The shim does not link it, so call nothing else here unless this
 * answered VIRTX_FUSE_T_OK. Thread-safe. */
int virtx_fuse_t_status(void);

/* The loaded libfuse-t's `fuse_version()`, or 0 when none was loaded. */
int virtx_fuse_t_api(void);

/* The loaded FUSE-T release, as its installer names the file
 * (`libfuse-t-<release>.dylib`), or "" when that name does not say. */
const char *virtx_fuse_t_release(void);

/* The FUSE-T release `fuse_t.h` was last checked against. */
const char *virtx_fuse_t_checked(void);

/* Mount and build a session. Returns NULL on failure. The returned pointer owns
 * the channel and session and must be freed with `virtx_fuse_t_destroy`.
 *
 * `backend` is FUSE-T's transport ("nfs", "smb" or "fskit"), or NULL for
 * `fuse-t.ini`'s choice (default nfs). The same vtable answers either way. */
void *virtx_fuse_t_mount(const char *mountpoint, const char *fsname,
                          const char *backend, void *fs,
                          const struct virtx_fuse_t_ops *ops);

/* Serve requests until the session ends. Blocks; call from a dedicated thread. */
int virtx_fuse_t_loop(void *session);

/* End the serving loop so `virtx_fuse_t_loop` returns and its thread can be
 * joined. Idempotent.
 *
 * **Does not unmount.** `fuse_unmount` breaks with a second mount alive: it
 * ends in a blocking `waitpid` on a process-global pid every mount overwrites,
 * so it waits on another session's helper. The caller unmounts through the
 * operating system instead. */
void virtx_fuse_t_stop(void *session);

/* Release the session and channel. Must not run while `virtx_fuse_t_loop` does. */
void virtx_fuse_t_destroy(void *session);

#endif
