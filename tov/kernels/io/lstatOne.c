#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
/* kind u8 | size u64 | mtime u32 of a path, as the folder listing gives them
 * (without the hidden and mount flags), or "" if it isn't there. */
static int ls_stat(const char *path, uint8_t out[13]) {
    struct stat st;
    if (lstat(path, &st) != 0) return 0;
    unsigned t = st.st_mode & S_IFMT;
    uint8_t kind = t == S_IFREG ? 0 : t == S_IFDIR ? 1 : t == S_IFLNK ? 2 : 3;
    uint64_t size = (uint64_t)st.st_size;
    int64_t m = (int64_t)st.st_mtime;
    uint32_t mtime = m < 0 ? 0 : m > (int64_t)UINT32_MAX ? UINT32_MAX : (uint32_t)m;
    out[0] = kind;
    memcpy(out + 1, &size, 8);
    memcpy(out + 9, &mtime, 4);
    return 1;
}
{
    char *path = malloc((size_t)p0.len + 1);
    if (p0.len) memcpy(path, tv_arr_data(p0), (size_t)p0.len);
    path[p0.len] = 0;
    uint8_t b[13];
    int ok = ls_stat(path, b);
    free(path);
    return ok ? tv_str_from((const char *)b, 13) : TV_EMPTY_STR;
}
