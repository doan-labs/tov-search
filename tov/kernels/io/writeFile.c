#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
{
    const char *path = p0.p->data;
    char tmp[4096];
    if (snprintf(tmp, sizeof tmp, "%s.tmp", path) >= (int)sizeof tmp) return tv_str_from("path too long", 13);
    int fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
    const char *err = NULL;
    if (fd < 0) {
        err = strerror(errno);
    } else {
        const char *d = tv_arr_data(p1);
        for (int64_t w = 0; w < p1.len && !err;) {
            ssize_t n = write(fd, d + w, (size_t)(p1.len - w));
            if (n < 0 && errno != EINTR) err = strerror(errno);
            if (n > 0) w += n;
        }
        if (close(fd) != 0 && !err) err = strerror(errno);
        if (!err && rename(tmp, path) != 0) err = strerror(errno);
        if (err) unlink(tmp);
    }
    return err ? tv_str_from(err, strlen(err)) : TV_EMPTY_STR;
}
