#include "grams.h"
#include "par.h"
/* gramFill on every core: each file's grams into its slots (gramCount sized them). */

static void one(void *ctx, size_t k) {
    const k_gramFill_args *a = ctx;
    if (!a->gText[k]) return;
    gr_doc d = gr_grams(a->gBuf + a->gStart[k], a->gLen[k]);
    const uint32_t *at = a->gAt + 4 * k, *cnt = a->gCnt + 4 * k;
    memcpy(a->gTri + at[0], d.tri, d.ntri * sizeof *d.tri);
    memcpy(a->gSym + at[1], d.sym, d.nsym * sizeof *d.sym);
    uint64_t *bw = a->gBloom + at[2], *lw = a->gLong + at[3];
    memset(bw, 0, cnt[2] * sizeof *bw);
    memset(lw, 0, cnt[3] * sizeof *lw);
    for (size_t i = 0; i < d.nsh; i++)
        for (int s = 0; s < 2; s++) {
            size_t b = gr_bit(d.sh[i], cnt[2], s);
            bw[b / 64] |= 1ULL << (b % 64);
        }
    for (size_t i = 0; i < d.nlg; i++)
        for (int s = 0; s < 2; s++) {
            size_t b = gr_bit(d.lg[i], cnt[3], s);
            lw[b / 64] |= 1ULL << (b % 64);
        }
    gr_free(&d);
}

void k_gramFill(const k_gramFill_args *a) {
    par_for((size_t)a->n, (void *)a, one);
}
