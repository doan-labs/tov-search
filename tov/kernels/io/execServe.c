#include <errno.h>
#include <string.h>
#include <unistd.h>
{
    char self[PATH_MAX];
    const char *exe = ss_self(self); /* in spawnServe.c */
    if (*exe) execl(exe, exe, "serve", (char *)NULL);
    return tv_str_from(strerror(errno), strlen(strerror(errno)));
}
