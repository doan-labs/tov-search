#include <limits.h>
#include <stdlib.h>
#include <string.h>
{
    char buf[PATH_MAX];
    if (!realpath(p0.p->data, buf)) {
        tv_str_retain(p0);
        return p0;
    }
    return tv_str_from(buf, strlen(buf));
}
