#include <stdio.h>
#include <sys/types.h>
void tv_out_flush(void);
{
    static char *line;
    static size_t cap;
    tv_out_flush();
    ssize_t n = getline(&line, &cap, stdin);
    if (n < 0) return tv_str_from("\n", 1);
    if (n > 0 && line[n - 1] == '\n') line[--n] = 0;
    tvk_query++;
    tvk_line = line;
    return tv_str_from(line, (size_t)n);
}
