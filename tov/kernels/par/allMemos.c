#include <stdlib.h>
#include "par.h"
/* Dir memos on every core. A dir's descendants are one contiguous id range,
 * so the tree splits once into subtrees of at most 4096 dirs (folded in
 * parallel) and the dirs above them (folded first, one by one). The split is
 * the reference's memo_plan; the index never changes, so it is made once. */
typedef struct {
    const uint32_t *de, *dp, *en, *hs;
    const uint8_t *hf, *hb;
    const int32_t *h[4];
    uint32_t *ms;
    int64_t *mb;
    int32_t *m[4];
    uint32_t gen;
    const int64_t *chunk; /* (first, end) dir ids per chunk */
} tvm_ctx;

static inline void tvm_one(const tvm_ctx *x, int64_t k) {
    uint32_t id = x->en[x->de[k]], p = x->dp[k];
    int64_t bits = 0;
    int32_t b[4] = {0, 0, 0, 0};
    if (x->hs[id] == x->gen) {
        if (x->hf[id] & 8) bits = -1;
        else {
            bits = x->hb[id];
            for (int t = 0; t < 4; t++) b[t] = x->h[t][id];
        }
    }
    if (x->mb[p] == -1 || bits == -1) bits = -1;
    else {
        bits |= x->mb[p];
        for (int t = 0; t < 4; t++) if (x->m[t][p] > b[t]) b[t] = x->m[t][p];
    }
    x->ms[k] = x->gen;
    x->mb[k] = bits;
    for (int t = 0; t < 4; t++) x->m[t][k] = b[t];
}

static void tvm_chunk(void *ctx, size_t ci) {
    const tvm_ctx *x = ctx;
    for (int64_t k = x->chunk[2 * ci]; k < x->chunk[2 * ci + 1]; k++) tvm_one(x, k);
}

static int64_t tvm_pp(const uint32_t *de, int64_t d, uint32_t v) { /* first dir id whose entry >= v */
    int64_t lo = 0, hi = d;
    while (lo < hi) {
        int64_t mid = (lo + hi) / 2;
        if (de[mid] < v) lo = mid + 1; else hi = mid;
    }
    return lo;
}
static int tvm_by_start(const void *a, const void *b) {
    int64_t x = ((const int64_t *)a)[0], y = ((const int64_t *)b)[0];
    return x < y ? -1 : x > y;
}
void k_allMemos(const k_allMemos_args *a_) {
    static int64_t *upper, nupper, *chunks, nchunks;
    const uint32_t *de = a_->dirEntry, *ds = a_->dirStart, *dl = a_->dirLen, *dend = a_->dirEnd;
    int64_t d = a_->d;
    if (!upper) {
        upper = malloc((size_t)(d + 1) * sizeof *upper);
        chunks = malloc((size_t)(2 * d + 2) * sizeof *chunks);
        int64_t *big = malloc((size_t)(d + 1) * sizeof *big), nbig = 0;
        big[nbig++] = 0;
        while (nbig) {
            int64_t k = big[--nbig];
            int64_t a = tvm_pp(de, d, ds[k]), b = tvm_pp(de, d, ds[k] + dl[k]);
            for (int64_t c = a; c < b; c++) {
                upper[nupper++] = c;
                int64_t r0 = tvm_pp(de, d, ds[c]), r1 = tvm_pp(de, d, dend[c]);
                if (r1 - r0 > 4096) big[nbig++] = c;
                else if (r0 < r1) { chunks[2 * nchunks] = r0; chunks[2 * nchunks + 1] = r1; nchunks++; }
            }
        }
        free(big);
        qsort(chunks, (size_t)nchunks, 2 * sizeof *chunks, tvm_by_start);
    }
    tvm_ctx x = {
        .de = de, .dp = a_->dirParent, .en = a_->entName, .hs = a_->hStamp,
        .hf = a_->hFlags, .hb = a_->hBits,
        .h = {a_->hBest0, a_->hBest1, a_->hBest2, a_->hBest3},
        .ms = a_->mStamp, .mb = a_->mBits,
        .m = {a_->mBest0, a_->mBest1, a_->mBest2, a_->mBest3},
        .gen = (uint32_t)a_->gen, .chunk = chunks,
    };
    x.ms[0] = x.gen; x.mb[0] = 0;
    for (int t = 0; t < 4; t++) x.m[t][0] = 0;
    for (int64_t j = 0; j < nupper; j++) tvm_one(&x, upper[j]);
    par_for((size_t)nchunks, &x, tvm_chunk);
}
