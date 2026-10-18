#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

/* The background program's connections: what each has sent so far. eof:
 * the client closed its side (its last line may still want an answer). */
typedef struct { int fd, eof; char *buf; size_t len, cap; } sk_conn;
static int sk_lfd = -1;
static sk_conn *sk_c;
static size_t sk_n, sk_cap;

static void sk_drop(size_t i) {
    close(sk_c[i].fd);
    free(sk_c[i].buf);
    sk_c[i] = sk_c[--sk_n];
}

/* The next non-blank line of connection i, as u32 fd + line; 0 if none yet. */
static int sk_line(size_t i, tv_str *out) {
    sk_conn *c = &sk_c[i];
    for (;;) {
        char *nl = memchr(c->buf, '\n', c->len);
        if (!nl && c->eof && c->len) {
            nl = c->buf + c->len; /* the last line, without its "\n" */
        }
        if (!nl) return 0;
        size_t l = (size_t)(nl - c->buf), used = l < c->len ? l + 1 : l;
        size_t a = 0, b = l;
        while (a < b && (c->buf[a] == ' ' || c->buf[a] == '\t' || c->buf[a] == '\r')) a++;
        while (b > a && (c->buf[b - 1] == ' ' || c->buf[b - 1] == '\t' || c->buf[b - 1] == '\r')) b--;
        if (a == b) {
            memmove(c->buf, c->buf + used, c->len - used);
            c->len -= used;
            continue;
        }
        char *s = malloc(4 + l);
        uint32_t fd = (uint32_t)c->fd;
        memcpy(s, &fd, 4);
        memcpy(s + 4, c->buf, l);
        *out = tv_str_from(s, 4 + l);
        free(s);
        memmove(c->buf, c->buf + used, c->len - used);
        c->len -= used;
        return 1;
    }
}

static tv_str sk_next(void) {
    for (;;) {
        for (size_t i = 0; i < sk_n; i++) {
            tv_str s;
            if (sk_line(i, &s)) return s;
        }
        for (size_t i = sk_n; i-- > 0;)
            if (sk_c[i].eof) sk_drop(i);
        struct pollfd *p = malloc((sk_n + 1) * sizeof *p);
        p[0] = (struct pollfd){sk_lfd, POLLIN, 0};
        for (size_t i = 0; i < sk_n; i++) p[i + 1] = (struct pollfd){sk_c[i].fd, POLLIN, 0};
        size_t n = sk_n;
        if (poll(p, (nfds_t)(n + 1), -1) < 0) {
            free(p);
            continue;
        }
        for (size_t i = n; i-- > 0;) {
            if (!(p[i + 1].revents & (POLLIN | POLLHUP | POLLERR))) continue;
            sk_conn *c = &sk_c[i];
            if (c->cap - c->len < 65536) {
                c->cap = c->cap * 2 + 65536;
                c->buf = realloc(c->buf, c->cap);
            }
            ssize_t r = read(c->fd, c->buf + c->len, c->cap - c->len);
            if (r > 0) c->len += (size_t)r;
            else if (r == 0 || errno != EINTR) c->eof = 1;
        }
        if (p[0].revents & POLLIN) {
            int fd = accept(sk_lfd, NULL, NULL);
            if (fd >= 0) {
                fcntl(fd, F_SETFD, FD_CLOEXEC);
                if (sk_n == sk_cap) {
                    sk_cap = sk_cap * 2 + 16;
                    sk_c = realloc(sk_c, sk_cap * sizeof *sk_c);
                }
                sk_c[sk_n++] = (sk_conn){fd, 0, NULL, 0, 0};
            }
        }
        free(p);
    }
}

static void sk_reply(int64_t fd, const char *d, int64_t n) {
    for (int64_t w = 0; w < n;) {
        ssize_t r = write((int)fd, d + w, (size_t)(n - w));
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) {
            for (size_t i = 0; i < sk_n; i++)
                if (sk_c[i].fd == fd) sk_c[i].eof = 1, sk_c[i].len = 0;
            return;
        }
        w += r;
    }
}
{
    signal(SIGPIPE, SIG_IGN);
    int lk = open(p1.p->data, O_WRONLY | O_CREAT | O_CLOEXEC, 0644);
    if (lk < 0) return tv_str_from(strerror(errno), strlen(strerror(errno)));
    if (flock(lk, LOCK_EX | LOCK_NB) != 0) {
        close(lk);
        return tv_str_from("another tov-search daemon is running", 36);
    }
    /* lk stays open: the lock lasts as long as this process. */
    struct sockaddr_un a = {0};
    a.sun_family = AF_UNIX;
    if (strlen(p0.p->data) >= sizeof a.sun_path) return tv_str_from("socket path too long", 20);
    strcpy(a.sun_path, p0.p->data);
    unlink(a.sun_path);
    sk_lfd = socket(AF_UNIX, SOCK_STREAM | 0, 0);
    if (sk_lfd < 0 || bind(sk_lfd, (struct sockaddr *)&a, sizeof a) != 0 || listen(sk_lfd, 128) != 0)
        return tv_str_from(strerror(errno), strlen(strerror(errno)));
    fcntl(sk_lfd, F_SETFD, FD_CLOEXEC);
    return TV_EMPTY_STR;
}
