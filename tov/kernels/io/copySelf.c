#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>
{
    char self[PATH_MAX];
    const char *exe = ss_self(self); /* in spawnServe.c */
    if (!*exe) return tv_str_from("cannot find this program's file", 31);
    int in = open(exe, O_RDONLY | O_CLOEXEC);
    if (in < 0) return tv_str_from(strerror(errno), strlen(strerror(errno)));
    unlink(p0.p->data);
    int out = open(p0.p->data, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0755);
    const char *err = out < 0 ? strerror(errno) : NULL;
    char buf[1 << 16];
    for (ssize_t r; !err && (r = read(in, buf, sizeof buf)) != 0;) {
        if (r < 0) {
            if (errno != EINTR) err = strerror(errno);
            continue;
        }
        for (ssize_t w = 0; !err && w < r;) {
            ssize_t k = write(out, buf + w, (size_t)(r - w));
            if (k < 0 && errno != EINTR) err = strerror(errno);
            if (k > 0) w += k;
        }
    }
    close(in);
    if (out >= 0 && close(out) != 0 && !err) err = strerror(errno);
    return err ? tv_str_from(err, strlen(err)) : TV_EMPTY_STR;
}
