/* The part of libfuse-t's interface the shim uses, declared here so that the shim builds
 * without FUSE-T installed.
 *
 * The shim opens libfuse-t with `dlopen` and calls it only through pointers `dlsym`
 * fills, so what it needs at build time is not the library but its types: the layouts it
 * hands libfuse-t and reads back, and the signatures of the calls it makes. Those are
 * libfuse's 2.x low-level API (`FUSE_USE_VERSION 26`) as FUSE-T 1.2 has it on macOS, and
 * are written out below from that interface rather than taken from its headers.
 *
 * **A layout here that disagrees with the library's is silent memory corruption**, not a
 * compile error. `check-abi.sh` beside this file compiles `abi_check.c` against these and
 * against FUSE-T's own headers and compares every struct byte for byte, and every
 * signature; CI runs it wherever FUSE-T is installed. Change this file only with it.
 */

#ifndef VIRTX_FUSE_T_H
#define VIRTX_FUSE_T_H

#include <stddef.h>
#include <stdint.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/types.h>

/* The FUSE-T these declarations were last checked against with `check-abi.sh`, and the
 * releases the shim trusts them for: the same major version. A libfuse-t of another major
 * version, or of another libfuse API than 2.x, is refused before any call is made into it
 * -- a layout it disagrees with would be a crash, not an error -- unless
 * `VIRTX_FUSE_T_UNCHECKED` says to go ahead. */
#define VIRTX_FUSE_T_CHECKED "1.2.7"
#define VIRTX_FUSE_T_MAJOR 1
/* libfuse's own API version, `fuse_version()`: these are 2.x's, 2.6 to 2.9. */
#define VIRTX_FUSE_T_API_MIN 26
#define VIRTX_FUSE_T_API_MAX 29

typedef unsigned long fuse_ino_t;
typedef struct fuse_req *fuse_req_t;
struct fuse_chan;
struct fuse_session;

struct fuse_args {
    int argc;
    char **argv;
    int allocated;
};

#define FUSE_ARGS_INIT(argc, argv) { argc, argv, 0 }

struct fuse_file_info {
    int flags;
    unsigned long fh_old;
    int writepage;
    unsigned int direct_io : 1;
    unsigned int keep_cache : 1;
    unsigned int flush : 1;
    unsigned int nonseekable : 1;
    unsigned int flock_release : 1;
    unsigned int padding : 25;
    /* macOS only: after the padding, in the same 32-bit unit. */
    unsigned int purge_attr : 1;
    unsigned int purge_ubc : 1;
    uint64_t fh;
    uint64_t lock_owner;
};

struct fuse_entry_param {
    fuse_ino_t ino;
    unsigned long generation;
    struct stat attr;
    double attr_timeout;
    double entry_timeout;
};

/* `setattr`'s `to_set`. */
#define FUSE_SET_ATTR_SIZE (1 << 3)

/* A member the shim never sets. Every member is a function pointer, so its type does not
 * change the layout; only the position does. */
typedef void (*virtx_fuse_t_unused_op)(void);

/* Every member, in order, macOS ones included, so that `sizeof` is the library's: it is
 * the `op_size` handed to `fuse_lowlevel_new`. */
struct fuse_lowlevel_ops {
    virtx_fuse_t_unused_op init;
    virtx_fuse_t_unused_op destroy;
    void (*lookup)(fuse_req_t req, fuse_ino_t parent, const char *name);
    void (*forget)(fuse_req_t req, fuse_ino_t ino, unsigned long nlookup);
    void (*getattr)(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi);
    void (*setattr)(fuse_req_t req, fuse_ino_t ino, struct stat *attr, int to_set,
                    struct fuse_file_info *fi);
    virtx_fuse_t_unused_op readlink;
    virtx_fuse_t_unused_op mknod;
    void (*mkdir)(fuse_req_t req, fuse_ino_t parent, const char *name, mode_t mode);
    void (*unlink)(fuse_req_t req, fuse_ino_t parent, const char *name);
    void (*rmdir)(fuse_req_t req, fuse_ino_t parent, const char *name);
    virtx_fuse_t_unused_op symlink;
    void (*rename)(fuse_req_t req, fuse_ino_t parent, const char *name, fuse_ino_t newparent,
                   const char *newname);
    virtx_fuse_t_unused_op link;
    void (*open)(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi);
    void (*read)(fuse_req_t req, fuse_ino_t ino, size_t size, off_t off,
                 struct fuse_file_info *fi);
    void (*write)(fuse_req_t req, fuse_ino_t ino, const char *buf, size_t size, off_t off,
                  struct fuse_file_info *fi);
    void (*flush)(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi);
    void (*release)(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi);
    void (*fsync)(fuse_req_t req, fuse_ino_t ino, int datasync, struct fuse_file_info *fi);
    virtx_fuse_t_unused_op opendir;
    void (*readdir)(fuse_req_t req, fuse_ino_t ino, size_t size, off_t off,
                    struct fuse_file_info *fi);
    virtx_fuse_t_unused_op releasedir;
    virtx_fuse_t_unused_op fsyncdir;
    void (*statfs)(fuse_req_t req, fuse_ino_t ino);
    virtx_fuse_t_unused_op setxattr;
    virtx_fuse_t_unused_op getxattr;
    virtx_fuse_t_unused_op listxattr;
    virtx_fuse_t_unused_op removexattr;
    virtx_fuse_t_unused_op access;
    void (*create)(fuse_req_t req, fuse_ino_t parent, const char *name, mode_t mode,
                   struct fuse_file_info *fi);
    virtx_fuse_t_unused_op getlk;
    virtx_fuse_t_unused_op setlk;
    virtx_fuse_t_unused_op bmap;
    virtx_fuse_t_unused_op ioctl;
    virtx_fuse_t_unused_op poll;
    virtx_fuse_t_unused_op write_buf;
    virtx_fuse_t_unused_op retrieve_reply;
    virtx_fuse_t_unused_op forget_multi;
    virtx_fuse_t_unused_op flock;
    virtx_fuse_t_unused_op fallocate;
    virtx_fuse_t_unused_op reserved00;
    virtx_fuse_t_unused_op reserved01;
    virtx_fuse_t_unused_op monitor;
    virtx_fuse_t_unused_op renamex;
    virtx_fuse_t_unused_op setvolname;
    virtx_fuse_t_unused_op exchange;
    virtx_fuse_t_unused_op getxtimes;
    virtx_fuse_t_unused_op setattr_x;
};

/* The calls the shim makes. Declared only so that `__typeof__` gives each pointer its
 * type: nothing links against these names. */
int fuse_version(void);
size_t fuse_add_direntry(fuse_req_t req, char *buf, size_t bufsize, const char *name,
                         const struct stat *stbuf, off_t off);
int fuse_chan_fd(struct fuse_chan *ch);
struct fuse_session *fuse_lowlevel_new(struct fuse_args *args,
                                       const struct fuse_lowlevel_ops *op, size_t op_size,
                                       void *userdata);
struct fuse_chan *fuse_mount(const char *mountpoint, struct fuse_args *args);
int fuse_opt_add_arg(struct fuse_args *args, const char *arg);
void fuse_opt_free_args(struct fuse_args *args);
int fuse_reply_attr(fuse_req_t req, const struct stat *attr, double attr_timeout);
int fuse_reply_buf(fuse_req_t req, const char *buf, size_t size);
int fuse_reply_create(fuse_req_t req, const struct fuse_entry_param *e,
                      const struct fuse_file_info *fi);
int fuse_reply_entry(fuse_req_t req, const struct fuse_entry_param *e);
int fuse_reply_err(fuse_req_t req, int err);
void fuse_reply_none(fuse_req_t req);
int fuse_reply_open(fuse_req_t req, const struct fuse_file_info *fi);
int fuse_reply_statfs(fuse_req_t req, const struct statvfs *stbuf);
int fuse_reply_write(fuse_req_t req, size_t count);
void *fuse_req_userdata(fuse_req_t req);
void fuse_session_add_chan(struct fuse_session *se, struct fuse_chan *ch);
void fuse_session_destroy(struct fuse_session *se);
void fuse_session_exit(struct fuse_session *se);
int fuse_session_loop(struct fuse_session *se);
void fuse_unmount(const char *mountpoint, struct fuse_chan *ch);

#endif
