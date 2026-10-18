#include <errno.h>
#include <unistd.h>
void tv_out_flush(void);
{
    tv_out_flush();
    const char *d = tv_arr_data(p0);
    for (int64_t w = 0; w < p0.len;) {
        ssize_t n = write(1, d + w, (size_t)(p0.len - w));
        if (n < 0 && errno != EINTR) return;
        if (n > 0) w += n;
    }
}
