#include <errno.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#ifndef __APPLE__
#include <dirent.h>
#include <fcntl.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#endif

/* Events waiting for watchTake, each: flags u32 | stream u32 | id u64 |
 * path length u32 | path. Every push wakes sockNext (sk_wake, sockListen.c). */
static pthread_mutex_t wt_mu = PTHREAD_MUTEX_INITIALIZER;
static char *wt_buf;
static size_t wt_len, wt_cap;

static void wt_push(uint32_t flags, uint32_t stream, uint64_t id, const char *path, size_t n) {
    uint32_t l = (uint32_t)n;
    pthread_mutex_lock(&wt_mu);
    if (wt_len + 20 + n > wt_cap) {
        wt_cap = (wt_len + 20 + n) * 2;
        wt_buf = realloc(wt_buf, wt_cap);
    }
    memcpy(wt_buf + wt_len, &flags, 4);
    memcpy(wt_buf + wt_len + 4, &stream, 4);
    memcpy(wt_buf + wt_len + 8, &id, 8);
    memcpy(wt_buf + wt_len + 16, &l, 4);
    memcpy(wt_buf + wt_len + 20, path, n);
    wt_len += 20 + n;
    pthread_mutex_unlock(&wt_mu);
    if (sk_wake[1] >= 0) {
        ssize_t r = write(sk_wake[1], "x", 1);
        (void)r; /* a full pipe already wakes it */
    }
}

static tv_str wt_take(void) {
    pthread_mutex_lock(&wt_mu);
    tv_str s = tv_str_from(wt_buf ? wt_buf : "", wt_len);
    wt_len = 0;
    pthread_mutex_unlock(&wt_mu);
    return s;
}

#ifdef __APPLE__
/* FSEvents, declared by hand like the Rust app does: CoreServices' headers
 * would bring thousands of names into Tov's C. */
typedef void (*wt_cb)(const void *s, void *info, size_t n, void *paths, const uint32_t *flags, const uint64_t *ids);
typedef struct { long version; void *info; const void *retain, *release, *desc; } wt_ctx;
extern void *FSEventStreamCreate(const void *alloc, wt_cb cb, const wt_ctx *ctx, const void *paths, uint64_t since, double latency, uint32_t flags);
extern void FSEventStreamSetDispatchQueue(void *s, void *q);
extern unsigned char FSEventStreamStart(void *s);
extern void FSEventStreamStop(void *s);
extern void FSEventStreamInvalidate(void *s);
extern void FSEventStreamRelease(void *s);
extern uint64_t FSEventsGetCurrentEventId(void);
extern const void *CFStringCreateWithCString(const void *alloc, const char *s, uint32_t enc);
extern const void *CFArrayCreate(const void *alloc, const void **vals, long n, const void *cbs);
extern const char kCFTypeArrayCallBacks;
extern void *dispatch_queue_create(const char *label, const void *attr);
extern const void *dispatch_queue_attr_make_with_qos_class(const void *attr, unsigned qos, int prio);

static void *wt_stream, *wt_queue;
static uint32_t wt_streams;

static void wt_events(const void *s, void *info, size_t n, void *paths, const uint32_t *flags, const uint64_t *ids) {
    (void)s;
    uint32_t stream = (uint32_t)(uintptr_t)info;
    char **p = paths;
    for (size_t i = 0; i < n; i++) wt_push(flags[i], stream, ids[i], p[i], strlen(p[i]));
}

/* Watch / from event id since (directory events, 0.1 s latency), replacing
 * the last stream. Returns the new stream's number. */
static int64_t wt_start(uint64_t since, const uint8_t *skip, int64_t skip_len) {
    (void)skip;
    (void)skip_len;
    if (wt_stream) {
        FSEventStreamStop(wt_stream);
        FSEventStreamInvalidate(wt_stream);
        FSEventStreamRelease(wt_stream);
    }
    if (!wt_queue) wt_queue = dispatch_queue_create("tov-search.fsevents", dispatch_queue_attr_make_with_qos_class(NULL, 0x19 /* user-initiated */, 0));
    const void *root = CFStringCreateWithCString(NULL, "/", 0x08000100 /* UTF-8 */);
    const void *arr = CFArrayCreate(NULL, &root, 1, &kCFTypeArrayCallBacks);
    wt_ctx ctx = {0, (void *)(uintptr_t)++wt_streams, NULL, NULL, NULL};
    /* Not IgnoreSelf: the app's own renames are what its search must see. */
    wt_stream = FSEventStreamCreate(NULL, wt_events, &ctx, arr, since, 0.1, 0x2 /* NoDefer */);
    FSEventStreamSetDispatchQueue(wt_stream, wt_queue);
    FSEventStreamStart(wt_stream);
    return wt_streams;
}

static uint64_t wt_now(void) {
    return FSEventsGetCurrentEventId();
}

static int64_t wt_add(const uint8_t *req, int64_t len) {
    (void)req;
    (void)len;
    return 0; /* FSEvents watches the whole disk */
}
#else
/* inotify: one watch per folder, reported per folder like FSEvents, so the
 * same Tov code applies both. Flags as FSEvents': must scan subdirs 1, user
 * dropped 2, kernel dropped 4. */
static int wt_fd = -1;
static char **wt_paths; /* by watch descriptor */
static size_t wt_npaths;
static int64_t wt_full; /* folders not watched: the watch limit ran out */
static char *wt_skip;
static size_t wt_skip_len;

static uint64_t wt_now(void) {
    struct timespec t;
    clock_gettime(CLOCK_REALTIME, &t);
    return (uint64_t)t.tv_sec * 1000000000u + (uint64_t)t.tv_nsec;
}

static int wt_blocked(const char *p) {
    size_t n = strlen(p);
    for (size_t at = 0; at < wt_skip_len;) {
        uint32_t l;
        memcpy(&l, wt_skip + at, 4);
        const char *s = wt_skip + at + 4;
        at += 4 + l;
        if (n >= l && memcmp(p, s, l) == 0 && (n == l || p[l] == '/')) return 1;
    }
    return 0;
}

/* Watch one folder; 0 only when the watch limit is used up. */
static int wt_watch(const char *path) {
    int wd = inotify_add_watch(wt_fd, path, IN_CREATE | IN_DELETE | IN_MOVED_FROM | IN_MOVED_TO | IN_MODIFY | IN_CLOSE_WRITE |
                                                IN_ATTRIB | IN_DELETE_SELF | IN_ONLYDIR | IN_DONT_FOLLOW | IN_EXCL_UNLINK);
    if (wd < 0) return errno != ENOSPC;
    pthread_mutex_lock(&wt_mu);
    if ((size_t)wd >= wt_npaths) {
        size_t m = (size_t)wd * 2 + 64;
        wt_paths = realloc(wt_paths, m * sizeof *wt_paths);
        memset(wt_paths + wt_npaths, 0, (m - wt_npaths) * sizeof *wt_paths);
        wt_npaths = m;
    }
    free(wt_paths[wd]);
    wt_paths[wd] = strdup(path);
    pthread_mutex_unlock(&wt_mu);
    return 1;
}

/* A folder that just appeared and every folder under it, watched before its
 * parent's event goes out, so the scan of it starts after the watches exist. */
static void wt_tree(const char *root) {
    size_t n = 1, cap = 16;
    char **stack = malloc(cap * sizeof *stack);
    stack[0] = strdup(root);
    while (n) {
        char *p = stack[--n];
        if (!wt_blocked(p) && wt_watch(p)) {
            DIR *d = opendir(p);
            for (struct dirent *e; d && (e = readdir(d));) {
                if (e->d_type != DT_DIR || !strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
                size_t l = strlen(p);
                char *c = malloc(l + strlen(e->d_name) + 2);
                sprintf(c, "%s%s%s", p, l == 1 ? "" : "/", e->d_name);
                if (n == cap) stack = realloc(stack, (cap *= 2) * sizeof *stack);
                stack[n++] = c;
            }
            if (d) closedir(d);
        } else if (!wt_blocked(p)) {
            __atomic_add_fetch(&wt_full, 1, __ATOMIC_RELAXED);
        }
        free(p);
    }
    free(stack);
}

static void *wt_read(void *arg) {
    (void)arg;
    char *buf = malloc(256 * 1024);
    for (;;) {
        ssize_t n = read(wt_fd, buf, 256 * 1024);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) break;
        uint64_t id = wt_now();
        for (ssize_t p = 0; p + 16 <= n;) {
            struct inotify_event *e = (struct inotify_event *)(buf + p);
            p += 16 + e->len;
            if (e->mask & IN_Q_OVERFLOW) {
                wt_push(1 | 4, 0, id, "/", 1);
                continue;
            }
            pthread_mutex_lock(&wt_mu);
            char *dir = e->wd >= 0 && (size_t)e->wd < wt_npaths && wt_paths[e->wd] ? strdup(wt_paths[e->wd]) : NULL;
            if (dir && (e->mask & IN_IGNORED)) {
                free(wt_paths[e->wd]);
                wt_paths[e->wd] = NULL;
            }
            pthread_mutex_unlock(&wt_mu);
            if (!dir || (e->mask & (IN_IGNORED | IN_DELETE_SELF))) { /* its parent reports a removal */
                free(dir);
                continue;
            }
            if ((e->mask & IN_ISDIR) && (e->mask & (IN_CREATE | IN_MOVED_TO))) {
                size_t l = strlen(dir);
                char *c = malloc(l + strlen(e->name) + 2);
                sprintf(c, "%s%s%s", dir, l == 1 ? "" : "/", e->name);
                wt_tree(c);
                free(c);
            }
            wt_push(0, 0, id, dir, strlen(dir));
            free(dir);
        }
    }
    free(buf);
    return NULL;
}

static int64_t wt_start(uint64_t since, const uint8_t *skip, int64_t skip_len) {
    (void)since;
    if (wt_fd >= 0) return 0;
    wt_skip = malloc((size_t)skip_len + 1);
    memcpy(wt_skip, skip, (size_t)skip_len);
    wt_skip_len = (size_t)skip_len;
    wt_fd = inotify_init1(IN_CLOEXEC);
    if (wt_fd < 0) return -1;
    pthread_t th;
    pthread_create(&th, NULL, wt_read, NULL);
    pthread_detach(th);
    return 0;
}

/* Watch each folder of req (u32 length + path each); returns how many could
 * not be watched for the watch limit, in total so far. */
static int64_t wt_add(const uint8_t *req, int64_t len) {
    if (wt_fd < 0) return 1;
    for (int64_t p = 0; p < len;) {
        uint32_t l;
        memcpy(&l, req + p, 4);
        char *path = malloc(l + 1);
        memcpy(path, req + p + 4, l);
        path[l] = 0;
        p += 4 + (int64_t)l;
        if (!wt_watch(path)) __atomic_add_fetch(&wt_full, 1, __ATOMIC_RELAXED);
        free(path);
    }
    return __atomic_load_n(&wt_full, __ATOMIC_RELAXED);
}
#endif
{
    return wt_start((uint64_t)p0, tv_arr_data(p1), p1.len);
}
