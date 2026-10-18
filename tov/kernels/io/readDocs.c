#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
typedef void (*par_fn)(void *ctx, size_t i);
void par_for(size_t n, void *ctx, par_fn fn);
#ifdef __APPLE__
int setiopolicy_np(int iotype, int scope, int policy);
#endif

/* One file to read, then what was read (len 0xFFFFFFFF: not a regular file
 * we could open). */
typedef struct { const char *path; uint32_t plen; uint8_t *data; uint32_t len; } rd_doc;
typedef struct { rd_doc *docs; size_t max; } rd_job;

static void rd_one(void *ctx, size_t i) {
    rd_job *j = ctx;
    rd_doc *d = &j->docs[i];
    d->len = UINT32_MAX;
    char *path = malloc(d->plen + 1);
    memcpy(path, d->path, d->plen);
    path[d->plen] = 0;
    /* O_NONBLOCK: a FIFO can't hang open(); O_NOFOLLOW: links aren't followed. */
    int fd = open(path, O_RDONLY | O_NONBLOCK | O_NOFOLLOW | O_CLOEXEC);
    free(path);
    if (fd < 0) return;
    struct stat st;
    if (fstat(fd, &st) != 0 || !S_ISREG(st.st_mode)) {
        close(fd);
        return;
    }
    size_t want = (size_t)st.st_size + 1 < j->max ? (size_t)st.st_size + 1 : j->max;
#ifdef POSIX_FADV_SEQUENTIAL
    posix_fadvise(fd, 0, (off_t)want, POSIX_FADV_SEQUENTIAL);
#endif
    size_t cap = want ? want : 1, n = 0;
    uint8_t *b = malloc(cap);
    for (;;) {
        if (n == cap) {
            if (cap >= j->max) break;
            cap = cap * 2 < j->max ? cap * 2 : j->max;
            b = realloc(b, cap);
        }
        ssize_t r = read(fd, b + n, cap - n);
        if (r <= 0) break;
        n += (size_t)r;
    }
    close(fd);
    d->data = b;
    d->len = (uint32_t)n;
}
{
    /* iCloud placeholders fail fast instead of being downloaded because we
     * searched (IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, whole process, off). */
#ifdef __APPLE__
    static int rd_policy;
    if (!rd_policy) {
        setiopolicy_np(3, 0, 1);
        rd_policy = 1;
    }
#endif
    const uint8_t *req = tv_arr_data(p0);
    size_t n = 0;
    for (int64_t p = 0; p < p0.len; n++) {
        uint32_t l;
        memcpy(&l, req + p, 4);
        p += 4 + (int64_t)l;
    }
    rd_doc *docs = calloc(n ? n : 1, sizeof *docs);
    size_t k = 0;
    for (int64_t p = 0; p < p0.len; k++) {
        uint32_t l;
        memcpy(&l, req + p, 4);
        docs[k].path = (const char *)req + p + 4;
        docs[k].plen = l;
        p += 4 + (int64_t)l;
    }
    rd_job job = {docs, (size_t)p1};
    par_for(n, &job, rd_one);
    size_t total = 0;
    for (k = 0; k < n; k++) total += 4 + (docs[k].len == UINT32_MAX ? 0 : docs[k].len);
    char *out = malloc(total ? total : 1), *w = out;
    for (k = 0; k < n; k++) {
        memcpy(w, &docs[k].len, 4);
        w += 4;
        if (docs[k].len != UINT32_MAX) {
            memcpy(w, docs[k].data, docs[k].len);
            w += docs[k].len;
        }
        free(docs[k].data);
    }
    tv_str s = tv_str_from(out, total);
    free(out);
    free(docs);
    return s;
}
