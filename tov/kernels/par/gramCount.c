#include "grams.h"
#include "par.h"
/* gramCount on every core: each file's grams, counted. */

static void one(void *ctx, size_t k) {
    const k_gramCount_args *a = ctx;
    uint32_t *c = a->gCnt + 4 * k;
    c[0] = c[1] = c[2] = c[3] = 0;
    if (!a->gText[k]) return;
    gr_doc d = gr_grams(a->gBuf + a->gStart[k], a->gLen[k]);
    c[0] = (uint32_t)d.ntri;
    c[1] = (uint32_t)d.nsym;
    c[2] = (uint32_t)((d.nsh * 3 + 63) / 64);
    size_t bits = a->gLen[k] >= 16384 ? 3 : 2;
    c[3] = (uint32_t)((d.nlg * bits + 63) / 64);
    gr_free(&d);
}

void k_gramCount(const k_gramCount_args *a) {
    par_for((size_t)a->n, (void *)a, one);
}
