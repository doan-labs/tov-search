#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
{
    /* The file, mapped read-only, behind a page that holds an immortal array
     * header (rc -1: never written to or freed), so Tov reads it as a u8[]. */
    int fd = open(p0.p->data, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return TV_EMPTY_ARR;
    struct stat st;
    if (fstat(fd, &st) != 0 || st.st_size <= 0) {
        close(fd);
        return TV_EMPTY_ARR;
    }
    size_t len = (size_t)st.st_size, pg = (size_t)sysconf(_SC_PAGESIZE);
    char *base = mmap(NULL, pg + len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    if (base == MAP_FAILED) {
        close(fd);
        return TV_EMPTY_ARR;
    }
    void *m = mmap(base + pg, len, PROT_READ, MAP_PRIVATE | MAP_FIXED, fd, 0);
    close(fd);
    if (m == MAP_FAILED) {
        munmap(base, pg + len);
        return TV_EMPTY_ARR;
    }
    tv_arrbuf *b = (tv_arrbuf *)(base + pg - sizeof(tv_arrbuf));
    b->rc = -1;
    b->cap = (int64_t)len;
    return (tv_arr){b, (int64_t)len};
}
