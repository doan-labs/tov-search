#include <fcntl.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>
{
    int64_t n = p0.len;
    if (n == 0) return -1;
    const tv_str *a = tv_arr_data(p0);
    char **argv = calloc((size_t)n + 1, sizeof *argv);
    for (int64_t i = 0; i < n; i++) argv[i] = a[i].p->data;
    pid_t pid = fork();
    if (pid == 0) {
        if (p1) {
            int null = open("/dev/null", O_WRONLY);
            if (null >= 0) dup2(null, 2);
        }
        execvp(argv[0], argv);
        _exit(127);
    }
    free(argv);
    if (pid < 0) return -1;
    int st;
    while (waitpid(pid, &st, 0) < 0) {
    }
    return WIFEXITED(st) ? WEXITSTATUS(st) : -1;
}
