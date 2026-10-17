#include <stdlib.h>
#include <string.h>
#include "par.h"
#include "simd.h"
/* The SIMD prefilter on every core: chunks of names scanned into their own
 * lists, joined in order. */
typedef struct {
    const k_prefilter_args *a;
    int64_t step;
    uint32_t **out;
    int64_t *n;
} ctx_t;

static void chunk(void *ctx, size_t ci) {
    const ctx_t *x = ctx;
    const k_prefilter_args *a = x->a;
    int64_t lo = (int64_t)ci * x->step, hi = lo + x->step < a->u ? lo + x->step : a->u;
    if (hi < lo) hi = lo;
    x->out[ci] = malloc((size_t)(hi - lo + 1) * sizeof(uint32_t));
    x->n[ci] = prefilter_range(a->umask, lo, hi, a->nt, a->tMask, a->tLoose, a->tStart, x->out[ci]);
}

int64_t k_prefilter(const k_prefilter_args *a) {
    size_t chunks = 32;
    ctx_t x = {.a = a, .step = (a->u + 31) / 32};
    x.out = calloc(chunks, sizeof *x.out);
    x.n = calloc(chunks, sizeof *x.n);
    par_for(chunks, &x, chunk);
    uint32_t *cand = a->cand; /* typed, so a change to cand's type fails to compile */
    int64_t c = 0;
    for (size_t i = 0; i < chunks; i++) {
        memcpy(cand + c, x.out[i], (size_t)x.n[i] * sizeof *cand);
        c += x.n[i];
        free(x.out[i]);
    }
    free(x.out);
    free(x.n);
    return c;
}
