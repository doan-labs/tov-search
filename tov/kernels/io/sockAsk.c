#include <errno.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

/* A connection to the unix socket at path, or -1. */
static int sa_connect(const char *path) {
    struct sockaddr_un a = {0};
    a.sun_family = AF_UNIX;
    if (strlen(path) >= sizeof a.sun_path) return -1;
    strcpy(a.sun_path, path);
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    if (connect(fd, (struct sockaddr *)&a, sizeof a) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}
{
    signal(SIGPIPE, SIG_IGN);
    int fd = sa_connect(p0.p->data);
    if (fd < 0) return TV_EMPTY_STR;
    size_t n = (size_t)p1.p->len;
    char *req = malloc(n + 1);
    memcpy(req, p1.p->data, n);
    req[n] = '\n';
    for (size_t w = 0; w <= n;) {
        ssize_t r = write(fd, req + w, n + 1 - w);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) break;
        w += (size_t)r;
    }
    free(req);
    size_t len = 0, cap = 65536;
    char *buf = malloc(cap);
    for (;;) {
        if (len == cap) buf = realloc(buf, cap *= 2);
        ssize_t r = read(fd, buf + len, cap - len);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) break;
        char *nl = memchr(buf + len, '\n', (size_t)r);
        len += (size_t)r;
        if (nl) {
            len = (size_t)(nl - buf);
            break;
        }
    }
    close(fd);
    tv_str s = tv_str_from(buf, len);
    free(buf);
    return s;
}
