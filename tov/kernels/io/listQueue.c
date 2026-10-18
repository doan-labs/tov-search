#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#ifdef __APPLE__
#include <sys/attr.h>
#include <sys/stat.h>
#include <sys/vnode.h>
#else
#include <dirent.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#endif
#include <pthread.h>

/* A folder to list, then its listing: count records (see scan.tov) in p. */
typedef struct ld_out {
    struct ld_out *next;
    uint32_t id, plen, count;
    char *path;
    uint8_t *p;
    size_t len, cap;
} ld_out;

static void ld_put(ld_out *o, const void *src, size_t n) {
    if (o->len + n > o->cap) {
        size_t c = o->cap ? o->cap * 2 : 4096;
        while (c < o->len + n) c *= 2;
        o->p = realloc(o->p, c);
        o->cap = c;
    }
    memcpy(o->p + o->len, src, n);
    o->len += n;
}

static void ld_entry(ld_out *o, uint8_t kind, uint64_t size, uint32_t mtime, const char *name, size_t nlen) {
    if (nlen == 0 || nlen > 65535) return;
    uint16_t l = (uint16_t)nlen;
    ld_put(o, &kind, 1); /* little-endian hosts only (arm64, x86-64) */
    ld_put(o, &size, 8);
    ld_put(o, &mtime, 4);
    ld_put(o, &l, 2);
    ld_put(o, name, nlen);
    o->count++;
}

static uint32_t ld_clamp(int64_t t) { return t < 0 ? 0 : t > (int64_t)UINT32_MAX ? UINT32_MAX : (uint32_t)t; }

#ifdef __APPLE__
/* getattrlistbulk: hundreds of entries per call with kind, size, mtime and
 * flags attached. Mount points (and autofs triggers) get FLAG_MOUNT. */
static __thread char *ld_buf;
static void ld_list(int fd, ld_out *o) {
    struct attrlist al = {0};
    al.bitmapcount = ATTR_BIT_MAP_COUNT;
    al.commonattr = ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME | ATTR_CMN_ERROR | ATTR_CMN_OBJTYPE | ATTR_CMN_MODTIME | ATTR_CMN_FLAGS;
    al.dirattr = ATTR_DIR_MOUNTSTATUS;
    al.fileattr = ATTR_FILE_DATALENGTH;
    if (!ld_buf) ld_buf = malloc(256 * 1024);
    for (;;) {
        int n = getattrlistbulk(fd, &al, ld_buf, 256 * 1024, 0);
        if (n <= 0) break;
        char *e = ld_buf;
        for (int i = 0; i < n; i++) {
            uint32_t len, common, dirattr, fileattr;
            memcpy(&len, e, 4);
            memcpy(&common, e + 4, 4);
            memcpy(&dirattr, e + 12, 4);
            memcpy(&fileattr, e + 16, 4);
            char *f = e + 24;
            if (common & ATTR_CMN_ERROR) f += 4;
            if (common & ATTR_CMN_NAME) {
                attrreference_t ref;
                memcpy(&ref, f, sizeof ref);
                const char *name = f + ref.attr_dataoffset;
                size_t nlen = ref.attr_length ? ref.attr_length - 1 : 0;
                f += sizeof ref;
                uint8_t kind = 3;
                if (common & ATTR_CMN_OBJTYPE) {
                    uint32_t t;
                    memcpy(&t, f, 4);
                    kind = t == VREG ? 0 : t == VDIR ? 1 : t == VLNK ? 2 : 3;
                    f += 4;
                }
                uint32_t mtime = 0;
                if (common & ATTR_CMN_MODTIME) {
                    int64_t s;
                    memcpy(&s, f, 8);
                    mtime = ld_clamp(s);
                    f += 16;
                }
                if (common & ATTR_CMN_FLAGS) {
                    uint32_t fl;
                    memcpy(&fl, f, 4);
                    if (fl & UF_HIDDEN) kind |= 4;
                    f += 4;
                }
                if (dirattr & ATTR_DIR_MOUNTSTATUS) {
                    uint32_t m;
                    memcpy(&m, f, 4);
                    if (m & (DIR_MNTSTATUS_MNTPOINT | 0x2 /* DIR_MNTSTATUS_TRIGGER */)) kind |= 8;
                    f += 4;
                }
                uint64_t size = 0;
                if (fileattr & ATTR_FILE_DATALENGTH) memcpy(&size, f, 8);
                ld_entry(o, kind, size, mtime, name, nlen);
            }
            e += len;
        }
    }
}
#else
/* Mounts never entered: pseudo filesystems, other mounts' views (overlay,
 * squashfs snaps) and network or FUSE drives. statfs f_type values. */
static const uint32_t ld_skip_fs[] = {
    0x9fa0, 0x62656572, 0x1cd1, 0x0027e0eb, 0x63677270, 0x73636673, 0x64626720, 0x74726163, 0x6165676c,
    0xcafe4a11, 0x62656570, 0x19800202, 0x958458f6, 0xde5e81e4, 0x6e736673, 0x42494e4d, 0x65735543,
    0x0187, 0xf97cff8c, 0x794c7630, 0x73717368, 0x6969, 0x517b, 0xff534d42, 0xfe534d42, 0x65735546,
};
#ifndef STATX_ATTR_MOUNT_ROOT
#define STATX_ATTR_MOUNT_ROOT 0x00002000
#endif

/* A mount point worth entering: a local filesystem. */
static int ld_crossable(int fd, const char *name) {
    int m = openat(fd, name, O_PATH | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (m < 0) return 0;
    struct statfs sf;
    int ok = fstatfs(m, &sf) == 0;
    for (size_t i = 0; ok && i < sizeof ld_skip_fs / sizeof *ld_skip_fs; i++)
        if ((uint32_t)sf.f_type == ld_skip_fs[i]) ok = 0;
    close(m);
    return ok;
}

/* getdents64 plus one statx per entry. Real filesystems mounted below are
 * entered (a Linux disk is often /, /home, /boot...); the rest get FLAG_MOUNT. */
static __thread char *ld_buf;
static void ld_list(int fd, ld_out *o) {
    struct stat me;
    if (fstat(fd, &me) != 0) return;
    if (!ld_buf) ld_buf = malloc(64 * 1024);
    for (;;) {
        long n = syscall(SYS_getdents64, fd, ld_buf, 64 * 1024);
        if (n <= 0) break;
        for (long p = 0; p < n;) {
            unsigned short reclen;
            memcpy(&reclen, ld_buf + p + 16, 2);
            unsigned char dtype = (unsigned char)ld_buf[p + 18];
            const char *name = ld_buf + p + 19;
            p += reclen;
            size_t nlen = strlen(name);
            if (nlen == 0 || (name[0] == '.' && (nlen == 1 || (nlen == 2 && name[1] == '.')))) continue;
            struct statx st;
            uint8_t kind;
            uint64_t size = 0;
            uint32_t mtime = 0;
            if (statx(fd, name, AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_STATX_DONT_SYNC, STATX_TYPE | STATX_SIZE | STATX_MTIME, &st) == 0) {
                unsigned t = st.stx_mode & S_IFMT;
                kind = t == S_IFREG ? 0 : t == S_IFDIR ? 1 : t == S_IFLNK ? 2 : 3;
                /* Like getattrlistbulk: a link's size is its target's length. */
                if (kind == 0 || kind == 2) size = st.stx_size;
                mtime = ld_clamp(st.stx_mtime.tv_sec);
                int root = (st.stx_attributes_mask & st.stx_attributes & STATX_ATTR_MOUNT_ROOT) != 0;
                if (kind == 1 && (root || makedev(st.stx_dev_major, st.stx_dev_minor) != me.st_dev) && !ld_crossable(fd, name)) kind |= 8;
            } else if (errno == ENOENT) {
                continue;
            } else {
                /* Listable but not searchable (no x on the folder): name only. */
                kind = dtype == DT_REG ? 0 : dtype == DT_LNK ? 2 : dtype == DT_DIR ? 1 : 3;
            }
            ld_entry(o, kind, size, mtime, name, nlen);
        }
    }
}
#endif

/* Folders wait in todo for a worker (one per core); their listings wait in
 * done for listTake. busy counts folders queued or being listed. */
static pthread_mutex_t ld_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t ld_work = PTHREAD_COND_INITIALIZER, ld_ready = PTHREAD_COND_INITIALIZER;
static ld_out *ld_todo, *ld_done;
static size_t ld_busy;

static void *ld_worker(void *arg) {
    (void)arg;
#ifdef __APPLE__
    /* Never download iCloud placeholders: listing a dataless folder fails
     * fast instead. (IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, thread, off) */
    extern int setiopolicy_np(int, int, int);
    setiopolicy_np(3, 1, 1);
#endif
    for (;;) {
        pthread_mutex_lock(&ld_mu);
        while (!ld_todo) pthread_cond_wait(&ld_work, &ld_mu);
        ld_out *o = ld_todo;
        ld_todo = o->next;
        pthread_mutex_unlock(&ld_mu);
        int fd = open(o->path, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
        if (fd >= 0) {
            ld_list(fd, o);
            close(fd);
        } else {
            o->count = UINT32_MAX; /* could not be opened */
        }
        pthread_mutex_lock(&ld_mu);
        o->next = ld_done;
        ld_done = o;
        pthread_cond_signal(&ld_ready);
        pthread_mutex_unlock(&ld_mu);
    }
    return NULL;
}

static void ld_start(void) {
    long cores = sysconf(_SC_NPROCESSORS_ONLN);
    for (long t = 0; t < (cores > 1 ? cores : 1); t++) {
        pthread_t th;
        if (pthread_create(&th, NULL, ld_worker, NULL) != 0) break;
        pthread_detach(th);
    }
}

/* Every listing done so far (waiting for one if none is), or "" when no
 * folder is queued or being listed. Per folder: u32 id | u32 path length |
 * path | u32 count | records. */
static tv_str ld_take(void) {
    pthread_mutex_lock(&ld_mu);
    while (!ld_done && ld_busy) pthread_cond_wait(&ld_ready, &ld_mu);
    ld_out *all = ld_done;
    ld_done = NULL;
    size_t total = 0, n = 0;
    for (ld_out *o = all; o; o = o->next, n++) total += 12 + o->plen + o->len;
    ld_busy -= n;
    pthread_mutex_unlock(&ld_mu);
    char *buf = malloc(total ? total : 1), *w = buf;
    while (all) {
        ld_out *o = all;
        all = o->next;
        memcpy(w, &o->id, 4);
        memcpy(w + 4, &o->plen, 4);
        memcpy(w + 8, o->path, o->plen);
        memcpy(w + 8 + o->plen, &o->count, 4);
        if (o->len) memcpy(w + 12 + o->plen, o->p, o->len);
        w += 12 + o->plen + o->len;
        free(o->p);
        free(o->path);
        free(o);
    }
    tv_str s = tv_str_from(buf, total);
    free(buf);
    return s;
}
{
    static pthread_once_t once = PTHREAD_ONCE_INIT;
    pthread_once(&once, ld_start);
    const uint8_t *req = tv_arr_data(p0);
    for (int64_t p = 0; p < p0.len;) {
        ld_out *o = calloc(1, sizeof *o);
        memcpy(&o->id, req + p, 4);
        memcpy(&o->plen, req + p + 4, 4);
        o->path = malloc(o->plen + 1);
        memcpy(o->path, req + p + 8, o->plen);
        o->path[o->plen] = 0;
        p += 8 + (int64_t)o->plen;
        pthread_mutex_lock(&ld_mu);
        o->next = ld_todo;
        ld_todo = o;
        ld_busy++;
        pthread_cond_signal(&ld_work);
        pthread_mutex_unlock(&ld_mu);
    }
}
