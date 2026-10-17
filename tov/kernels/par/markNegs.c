#include <stdint.h>
#include "par.h"
/* markNegs on every core: each chunk of 64-name words is filled by one worker.
 * The match test is a C port of fitsT() and tokenMatches(). */
typedef struct {
    const k_markNegs_args *a;
    int64_t words, step;
} ctx_t;

static inline int fold(int b) { return (b >= 'A' && b <= 'Z') ? (b | 32) : b; }

static int starts(const uint8_t *s, int64_t wo, int64_t wl, const uint8_t *q, int64_t ql) {
    if (wl < ql) return 0;
    for (int64_t j = 0; j < ql; j++)
        if (fold(s[wo + j]) != q[j]) return 0;
    return 1;
}

static int matches(const k_markNegs_args *a, int64_t o, int64_t l, int64_t t) {
    const uint8_t *s = a->unames, *q = a->tText + a->tOff[t];
    int64_t ql = (int64_t)a->tOff[t + 1] - a->tOff[t];
    switch (a->tMode[t]) {
    case 0: /* fuzzy: the word's letters in order */
        for (int64_t i = 0, j = 0; i < l; i++)
            if (fold(s[o + i]) == q[j] && ++j == ql) return 1;
        return 0;
    case 1: /* exact: anywhere */
        for (int64_t i = 0; i + ql <= l; i++)
            if (starts(s, o + i, ql, q, ql)) return 1;
        return 0;
    case 2: /* prefix */
        return starts(s, o, l, q, ql);
    default: /* suffix */
        return l >= ql && starts(s, o + l - ql, ql, q, ql);
    }
}

static void chunk(void *ctx, size_t ci) {
    const ctx_t *x = ctx;
    const k_markNegs_args *a = x->a;
    int64_t w0 = (int64_t)ci * x->step, w1 = w0 + x->step < x->words ? w0 + x->step : x->words;
    for (int64_t w = w0; w < w1; w++) {
        uint64_t bits = 0;
        int64_t k1 = w * 64 + 64 < a->u ? w * 64 + 64 : a->u;
        for (int64_t k = w * 64; k < k1; k++) {
            uint64_t m = a->umask[k];
            int64_t o = a->uoff[k], l = (int64_t)a->uoff[k + 1] - o;
            for (int64_t t = 0; t < a->nt; t++) {
                uint64_t miss = a->tMask[t] & ~m;
                int fits = miss == 0 || (((miss & ~a->tLoose[t]) | (miss & (miss - 1))) == 0 && (m & a->tStart[t]) != 0);
                if (fits && matches(a, o, l, t)) {
                    bits |= UINT64_C(1) << (k & 63);
                    break;
                }
            }
        }
        a->negBits[w] = bits;
    }
}

void k_markNegs(const k_markNegs_args *a) {
    int64_t words = (int64_t)a->negBits_len;
    ctx_t x = {.a = a, .words = words, .step = (words + 255) / 256};
    if (words > 0) par_for(256, &x, chunk);
}
