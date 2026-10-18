#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>
#ifdef __APPLE__
#include <mach-o/dyld.h>
#endif

/* This program's file, or "" (into buf of PATH_MAX). */
static const char *ss_self(char *buf) {
#ifdef __APPLE__
    uint32_t n = PATH_MAX;
    if (_NSGetExecutablePath(buf, &n) != 0) return "";
#else
    ssize_t n = readlink("/proc/self/exe", buf, PATH_MAX - 1);
    if (n < 0) return "";
    buf[n] = 0;
#endif
    return buf;
}
{
    char self[PATH_MAX];
    const char *exe = ss_self(self);
    if (!*exe) return tv_str_from("cannot find this program's file", 31);
    pid_t pid = fork();
    if (pid < 0) return tv_str_from(strerror(errno), strlen(strerror(errno)));
    if (pid == 0) {
        /* Its own session: closing the terminal that started it doesn't kill it. */
        setsid();
        int log = open(p0.p->data, O_WRONLY | O_CREAT | O_APPEND, 0644);
        int null = open("/dev/null", O_RDONLY);
        if (null >= 0) dup2(null, 0);
        if (log >= 0) {
            dup2(log, 1);
            dup2(log, 2);
        }
        execl(exe, exe, "serve", (char *)NULL);
        _exit(127);
    }
    struct sockaddr_un a = {0};
    a.sun_family = AF_UNIX;
    if (strlen(p1.p->data) >= sizeof a.sun_path) return tv_str_from("socket path too long", 20);
    strcpy(a.sun_path, p1.p->data);
    for (int i = 0; i < 100; i++) {
        struct timespec t = {0, 30 * 1000000};
        nanosleep(&t, NULL);
        int fd = socket(AF_UNIX, SOCK_STREAM, 0);
        if (fd < 0) continue;
        int ok = connect(fd, (struct sockaddr *)&a, sizeof a) == 0;
        close(fd);
        if (ok) return TV_EMPTY_STR;
    }
    return tv_str_from("the background program did not start", 37);
}
