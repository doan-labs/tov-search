#include <stdlib.h>
#include <string.h>
typedef void (*par_fn)(void *ctx, size_t i);
void par_for(size_t n, void *ctx, par_fn fn);

typedef struct { const uint8_t *req; const size_t *at; uint32_t since; uint8_t *hit; } ms_job;

static void ms_one(void *ctx, size_t i) {
    ms_job *j = ctx;
    uint32_t l;
    memcpy(&l, j->req + j->at[i], 4);
    char *path = malloc(l + 1);
    memcpy(path, j->req + j->at[i] + 4, l);
    path[l] = 0;
    uint8_t b[13];
    uint32_t mtime;
    if (ls_stat(path, b)) { /* in lstatOne.c */
        memcpy(&mtime, b + 9, 4);
        j->hit[i] = mtime >= j->since;
    }
    free(path);
}
{
    const uint8_t *req = tv_arr_data(p0);
    size_t n = 0;
    for (int64_t p = 0; p < p0.len; n++) {
        uint32_t l;
        memcpy(&l, req + p, 4);
        p += 4 + (int64_t)l;
    }
    size_t *at = malloc((n ? n : 1) * sizeof *at);
    uint8_t *hit = calloc(n ? n : 1, 1);
    size_t k = 0;
    for (int64_t p = 0; p < p0.len; k++) {
        uint32_t l;
        memcpy(&l, req + p, 4);
        at[k] = (size_t)p;
        p += 4 + (int64_t)l;
    }
    ms_job job = {req, at, (uint32_t)p1, hit};
    par_for(n, &job, ms_one);
    tv_str s = tv_str_from((const char *)hit, n);
    free(at);
    free(hit);
    return s;
}
