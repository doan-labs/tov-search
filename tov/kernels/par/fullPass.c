#include <stdlib.h>
#include <string.h>
#include "par.h"
/* fullPass across every core: each worker keeps its own top k of one
 * slice of entries, then one sort picks the k best of their survivors. Keys
 * pack (score, !index) so one u64 compare orders by score, then lower index. */
typedef struct {
    const uint32_t *entName, *entMtime, *entParent;
    const uint64_t *okBits; /* names that pass as a match: 440 KB, stays in L2 */
    const uint8_t *entKind, *hFlags, *hBits;
    const int64_t *entSize, *mBits;
    const int32_t *hScore, *dirPrior, *mB[4];
    int64_t lo, hi, k, kind, sizeLo, sizeHi, mtLo, mtHi, npos, now, step;
    uint32_t gen;
    bool needDirs;
    uint64_t **keys;
    int64_t *nkeys;
} tvf_ctx;

typedef struct { const uint32_t *ids; uint64_t *bits; int64_t n, step; } tvf_bits_ctx;
static void tvf_bits(void *ctx, size_t ci) {
    const tvf_bits_ctx *x = ctx;
    int64_t a = (int64_t)ci * x->step, b = a + x->step < x->n ? a + x->step : x->n;
    for (int64_t j = a; j < b; j++)
        __atomic_fetch_or(&x->bits[x->ids[j] >> 6], UINT64_C(1) << (x->ids[j] & 63), __ATOMIC_RELAXED);
}

static int tvf_desc(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? 1 : x > y ? -1 : 0;
}

static void tvf_chunk(void *ctx, size_t ci) {
    const tvf_ctx *x = ctx;
    int64_t a = x->lo + (int64_t)ci * x->step, b = a + x->step < x->hi ? a + x->step : x->hi;
    int64_t cap = 2 * x->k > 64 ? 2 * x->k : 64, n = 0;
    uint64_t *buf = malloc((size_t)(cap + 1) * sizeof(uint64_t)), floor = 0;
    for (int64_t i = a; i < b; i++) {
        uint32_t id = x->entName[i];
        if (!(x->okBits[id >> 6] >> (id & 63) & 1)) continue;
        int kind = x->entKind[i];
        if (x->kind >= 0 && (kind & 3) != x->kind) continue;
        int64_t sz = x->entSize[i];
        if (sz < x->sizeLo || sz > x->sizeHi) continue;
        int64_t mt = x->entMtime[i];
        if (mt < x->mtLo || mt > x->mtHi) continue;
        int64_t score = x->hScore[id];
        uint32_t p = x->entParent[i];
        if (x->needDirs) {
            int64_t all = (INT64_C(1) << x->npos) - 1, mb = x->mBits[p], nb = x->hBits[id];
            if (mb == -1 || ((nb | mb) & all) != all) continue;
            for (int64_t t = 0; t < x->npos; t++)
                if (!(nb & (INT64_C(1) << t))) score += t < 4 ? x->mB[t][p] * 3 / 4 : 6;
        }
        int flags = x->hFlags[id], tw = 0;
        if (flags & 2) tw -= 8;
        if (kind & 4) tw -= 8;
        if (((kind & 3) == 1 || (kind & 3) == 2) && (flags & 4)) tw += 25;
        int64_t age = x->now > mt ? x->now - mt : 0;
        tw += age <= 86400 ? 10 : age <= 604800 ? 7 : age <= 2592000 ? 4 : age <= 31536000 ? 1 : 0;
        score += x->dirPrior[p] + tw;
        uint64_t key = ((uint64_t)(score - INT32_MIN) << 32) | (uint32_t)~(uint32_t)i;
        if (key <= floor) continue;
        buf[n++] = key;
        if (n >= cap) {
            qsort(buf, (size_t)n, sizeof *buf, tvf_desc);
            n = x->k;
            floor = buf[x->k - 1];
        }
    }
    qsort(buf, (size_t)n, sizeof *buf, tvf_desc);
    x->keys[ci] = buf;
    x->nkeys[ci] = n < x->k ? n : x->k;
}
int64_t k_fullPass(const k_fullPass_args *a) {
    int64_t k = a->topK;
    if (k <= 0 || a->hi <= a->lo) return 0;
    tvf_ctx x = {
        .entName = a->entName, .entMtime = a->entMtime, .entParent = a->entParent,
        .entKind = a->entKind, .hFlags = a->hFlags,
        .hBits = a->hBits, .entSize = a->entSize, .mBits = a->mBits,
        .hScore = a->hScore, .dirPrior = a->dirPrior,
        .mB = {a->mBest0, a->mBest1, a->mBest2, a->mBest3},
        .lo = a->lo, .hi = a->hi, .k = k, .kind = a->qKind, .sizeLo = a->qSizeLo, .sizeHi = a->qSizeHi,
        .mtLo = a->qMtimeLo, .mtHi = a->qMtimeHi, .npos = a->qNpos, .now = a->now,
        .gen = (uint32_t)a->gen, .needDirs = a->qNeedDirs,
    };
    /* The bitset of OK names is rebuilt only when the name table changed. */
    static uint64_t *okBits;
    static int64_t bitsGen = -1, bitsWords;
    int64_t words = a->u / 64 + 1;
    if (words > bitsWords) { /* the name table grew */
        free(okBits);
        okBits = malloc((size_t)words * sizeof(uint64_t));
        bitsWords = words;
        bitsGen = -1;
    }
    if (bitsGen != a->gen) {
        memset(okBits, 0, (size_t)words * sizeof(uint64_t));
        tvf_bits_ctx b = {.ids = a->okBuf, .bits = okBits, .n = a->okCount, .step = 1 << 16};
        par_for((size_t)((b.n + b.step - 1) / b.step), &b, tvf_bits);
        bitsGen = a->gen;
    }
    x.okBits = okBits;
    size_t chunks = 64;
    x.step = (a->hi - a->lo + (int64_t)chunks - 1) / (int64_t)chunks;
    x.keys = calloc(chunks, sizeof *x.keys);
    x.nkeys = calloc(chunks, sizeof *x.nkeys);
    par_for(chunks, &x, tvf_chunk);
    int64_t total = 0;
    for (size_t c = 0; c < chunks; c++) total += x.nkeys[c];
    uint64_t *all = malloc((size_t)(total + 1) * sizeof(uint64_t));
    int64_t at = 0;
    for (size_t c = 0; c < chunks; c++) {
        for (int64_t j = 0; j < x.nkeys[c]; j++) all[at++] = x.keys[c][j];
        free(x.keys[c]);
    }
    qsort(all, (size_t)total, sizeof *all, tvf_desc);
    int64_t got = total < k ? total : k;
    int64_t *ks = a->kScore, *ki = a->kIdx;
    for (int64_t j = 0; j < got; j++) {
        ks[j] = (int64_t)(all[j] >> 32) + INT32_MIN;
        ki[j] = (int64_t)(uint32_t)~(uint32_t)all[j];
    }
    free(all); free(x.keys); free(x.nkeys);
    return got;
}
