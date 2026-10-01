/* Prints the layout of every libfuse-t type the shim shares with the library, so that
 * `check-abi.sh` can compare `fuse_t.h` with FUSE-T's own headers: built once against each,
 * the two outputs have to be the same.
 *
 * A bitfield has no offset to print, so `fuse_file_info` is also printed as bytes, once
 * per field set on its own: that is the layout as the library reads it. */
#ifdef VIRTX_ABI_REAL
#define FUSE_USE_VERSION 26
#include <fuse_lowlevel.h>
#else
#include "fuse_t.h"
#endif

#include <stdio.h>
#include <string.h>

#define SIZE(t) printf("sizeof %s = %zu\n", #t, sizeof(t))
#define OFFSET(t, m) printf("offsetof %s.%s = %zu\n", #t, #m, offsetof(t, m))

static void bytes(const char *what, const void *p, size_t n) {
    printf("bytes %s =", what);
    for (size_t i = 0; i < n; i++) printf(" %02x", ((const unsigned char *)p)[i]);
    printf("\n");
}

#define BIT(m)                                                                          \
    do {                                                                                \
        struct fuse_file_info fi;                                                       \
        memset(&fi, 0, sizeof fi);                                                      \
        fi.m = 1;                                                                       \
        bytes("fuse_file_info." #m, &fi, sizeof fi);                                    \
    } while (0)

#define OPS(X)                                                                          \
    X(init) X(destroy) X(lookup) X(forget) X(getattr) X(setattr) X(readlink) X(mknod)   \
    X(mkdir) X(unlink) X(rmdir) X(symlink) X(rename) X(link) X(open) X(read) X(write)   \
    X(flush) X(release) X(fsync) X(opendir) X(readdir) X(releasedir) X(fsyncdir)        \
    X(statfs) X(setxattr) X(getxattr) X(listxattr) X(removexattr) X(access) X(create)   \
    X(getlk) X(setlk) X(bmap) X(ioctl) X(poll) X(write_buf) X(retrieve_reply)           \
    X(forget_multi) X(flock) X(fallocate) X(reserved00) X(reserved01) X(monitor)        \
    X(renamex) X(setvolname) X(exchange) X(getxtimes) X(setattr_x)
#define OP_OFFSET(m) OFFSET(struct fuse_lowlevel_ops, m);

int main(void) {
    SIZE(fuse_ino_t);
    SIZE(fuse_req_t);

    SIZE(struct fuse_args);
    OFFSET(struct fuse_args, argc);
    OFFSET(struct fuse_args, argv);
    OFFSET(struct fuse_args, allocated);
    struct fuse_args args = FUSE_ARGS_INIT(3, (char **)&args);
    printf("FUSE_ARGS_INIT argc=%d argv_is_given=%d allocated=%d\n", args.argc,
           args.argv == (char **)&args, args.allocated);

    SIZE(struct fuse_file_info);
    OFFSET(struct fuse_file_info, flags);
    OFFSET(struct fuse_file_info, fh_old);
    OFFSET(struct fuse_file_info, writepage);
    OFFSET(struct fuse_file_info, fh);
    OFFSET(struct fuse_file_info, lock_owner);
    BIT(direct_io);
    BIT(keep_cache);
    BIT(flush);
    BIT(nonseekable);
    BIT(flock_release);
    BIT(purge_attr);
    BIT(purge_ubc);

    SIZE(struct fuse_entry_param);
    OFFSET(struct fuse_entry_param, ino);
    OFFSET(struct fuse_entry_param, generation);
    OFFSET(struct fuse_entry_param, attr);
    OFFSET(struct fuse_entry_param, attr_timeout);
    OFFSET(struct fuse_entry_param, entry_timeout);

    SIZE(struct fuse_lowlevel_ops);
    OPS(OP_OFFSET)

    printf("FUSE_SET_ATTR_SIZE = %d\n", FUSE_SET_ATTR_SIZE);
    return 0;
}
