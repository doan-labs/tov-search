#include <sys/mman.h>
#include <unistd.h>
{
    /* Unmaps the file but keeps the header page: Tov still releases the
     * array (and so reads its header) after this, e.g. when segMap is reset.
     * ponytail: leaks one page per closed segment; fine while merges are rare. */
    if (p0.len == 0) return;
    munmap((char *)tv_arr_data(p0), (size_t)p0.len);
}
