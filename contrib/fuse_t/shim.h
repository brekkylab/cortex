/* The boundary between cortex and libfuse-t.
 *
 * `fuse_lowlevel_ops` (~50 function pointers, `__APPLE__`-conditional members),
 * `fuse_file_info` (bitfields) and `fuse_entry_param` (embeds a host
 * `struct stat`) stay in C, which has the real headers: a wrong layout in Rust
 * is silent memory corruption. Rust sees only the flat types below.
 *
 * Rust supplies `cortex_fuse_t_ops`. Operations return 0 or a negative errno,
 * as libfuse does.
 */

#ifndef CORTEX_FUSE_T_SHIM_H
#define CORTEX_FUSE_T_SHIM_H

#include <stddef.h>
#include <stdint.h>

/* A `struct stat` reduced to what cortex reports. Fixed-width with an explicit
 * pad so the Rust mirror is trivially correct; C widens it. */
struct cortex_stat {
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
typedef int (*cortex_dirent_sink)(void *sink, uint64_t ino, const char *name,
                                  uint32_t mode, uint64_t next_offset);

/* Implemented in Rust. `fs` is the opaque filesystem pointer handed to
 * `cortex_fuse_t_mount`. */
struct cortex_fuse_t_ops {
    int (*lookup)(void *fs, uint64_t parent, const char *name, uint64_t *ino,
                  struct cortex_stat *out);
    int (*getattr)(void *fs, uint64_t ino, struct cortex_stat *out);
    /* `has_size` distinguishes "resize to 0" from "size not being set". */
    int (*setattr)(void *fs, uint64_t ino, uint64_t fh, int has_fh, uint64_t size,
                   int has_size, struct cortex_stat *out);
    int (*open)(void *fs, uint64_t ino, int flags, uint64_t *fh);
    int (*create)(void *fs, uint64_t parent, const char *name, int flags,
                  uint64_t *ino, uint64_t *fh, struct cortex_stat *out);
    /* Byte counts on success, negative errno on failure. */
    long (*read)(void *fs, uint64_t fh, uint64_t offset, uint64_t size, char *buf);
    long (*write)(void *fs, uint64_t fh, uint64_t offset, uint64_t size,
                  const char *buf);
    int (*flush)(void *fs, uint64_t fh);
    int (*release)(void *fs, uint64_t fh);
    int (*mkdir)(void *fs, uint64_t parent, const char *name, uint64_t *ino,
                 struct cortex_stat *out);
    int (*unlink)(void *fs, uint64_t parent, const char *name);
    int (*rmdir)(void *fs, uint64_t parent, const char *name);
    /* No flags: libfuse-t's `rename` has none, so `RENAME_NOREPLACE`/
     * `RENAME_EXCHANGE` never reach Rust. */
    int (*rename)(void *fs, uint64_t parent, const char *name, uint64_t newparent,
                  const char *newname);
    int (*readdir)(void *fs, uint64_t ino, uint64_t offset, void *sink,
                   cortex_dirent_sink emit);
    void (*forget)(void *fs, uint64_t ino, uint64_t nlookup);
    /* Reported verbatim in the `statfs` reply. */
    uint64_t total_blocks;
    uint64_t total_inodes;
    uint32_t block_size;
    uint32_t name_max;
};

/* Open libfuse-t once per process; nonzero if it and every function the shim
 * calls were found. The shim does not link it, so call nothing else here unless
 * this answered nonzero. Thread-safe. */
int cortex_fuse_t_available(void);

/* Mount and build a session. Returns NULL on failure. The returned pointer owns
 * the channel and session and must be freed with `cortex_fuse_t_destroy`.
 *
 * `backend` is FUSE-T's transport ("nfs", "smb" or "fskit"), or NULL for
 * `fuse-t.ini`'s choice (default nfs). The same vtable answers either way. */
void *cortex_fuse_t_mount(const char *mountpoint, const char *fsname,
                          const char *backend, void *fs,
                          const struct cortex_fuse_t_ops *ops);

/* Serve requests until the session ends. Blocks; call from a dedicated thread. */
int cortex_fuse_t_loop(void *session);

/* End the serving loop so `cortex_fuse_t_loop` returns and its thread can be
 * joined. Idempotent.
 *
 * **Does not unmount.** `fuse_unmount` breaks with a second mount alive: it
 * ends in a blocking `waitpid` on a process-global pid every mount overwrites,
 * so it waits on another session's helper. The caller unmounts through the
 * operating system instead. */
void cortex_fuse_t_stop(void *session);

/* Release the session and channel. Must not run while `cortex_fuse_t_loop` does. */
void cortex_fuse_t_destroy(void *session);

#endif
