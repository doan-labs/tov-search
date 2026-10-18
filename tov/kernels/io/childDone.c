#include <sys/wait.h>
{
    /* -1 while child p0 runs, else its exit status (128 + signal if killed). */
    int st;
    pid_t r = waitpid((pid_t)p0, &st, WNOHANG);
    if (r == 0) return -1;
    if (r < 0) return 255;
    return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}
