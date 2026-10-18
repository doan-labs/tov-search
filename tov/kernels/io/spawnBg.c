#include <fcntl.h>
#include <limits.h>
#include <stdlib.h>
#include <unistd.h>
{
    /* This program with arguments p0, output appended to p1; its pid, or -1. */
    char self[PATH_MAX];
    const char *exe = ss_self(self); /* in spawnServe.c */
    if (!*exe) return -1;
    int64_t n = p0.len;
    const tv_str *a = tv_arr_data(p0);
    char **argv = calloc((size_t)n + 2, sizeof *argv);
    argv[0] = (char *)exe;
    for (int64_t i = 0; i < n; i++) argv[i + 1] = a[i].p->data;
    pid_t pid = fork();
    if (pid == 0) {
        int log = open(p1.p->data, O_WRONLY | O_CREAT | O_APPEND, 0644);
        int null = open("/dev/null", O_RDONLY);
        if (null >= 0) dup2(null, 0);
        if (log >= 0) {
            dup2(log, 1);
            dup2(log, 2);
        }
        execv(exe, argv);
        _exit(127);
    }
    free(argv);
    return pid < 0 ? -1 : (int64_t)pid;
}
