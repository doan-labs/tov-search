#include <time.h>
{
    struct timespec t = {(time_t)(p0 / 1000), (long)(p0 % 1000) * 1000000};
    while (nanosleep(&t, &t) != 0) {
    }
}
