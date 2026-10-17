#include <stdlib.h>
#include <string.h>
#include "par.h"
#include "simd.h"
/* scoreNamesLoop across every core: the name candidates split into chunks,
 * each scored by a worker into its own slots of the h* tables (names are
 * disjoint), its matches collected per chunk and joined in order after. The
 * scoring is a line-by-line C port of the Tov functions it replaces. */
#define TVP_NO (-1000000000)
enum { TVP_LOWER, TVP_UPPER, TVP_DIGIT, TVP_DELIM, TVP_OTHER };

static inline int tvp_fold(int b) { return (b >= 'A' && b <= 'Z') ? (b | 32) : b; }
static inline int tvp_cls(int b) {
    if (b >= 'a' && b <= 'z') return TVP_LOWER;
    if (b >= 'A' && b <= 'Z') return TVP_UPPER;
    if (b >= '0' && b <= '9') return TVP_DIGIT;
    switch (b) {
    case ' ': case '_': case '-': case '.': case '/': case '(': case ')': case '[': case ']': case ',': case '+': case '@':
        return TVP_DELIM;
    }
    return TVP_OTHER;
}
static inline int tvp_bonus(int prev, int cur) {
    if (prev == TVP_DELIM && cur != TVP_DELIM) return 8;
    if ((prev == TVP_LOWER && cur == TVP_UPPER) || ((prev == TVP_LOWER || prev == TVP_UPPER) && cur == TVP_DIGIT)) return 7;
    return 0;
}
static inline int tvp_lenpen(int64_t l) { return (int)((l < 80 ? l : 80) / 3); }
static inline int64_t tvp_stem(const uint8_t *s, int64_t o, int64_t l, int64_t off) {
    for (int64_t i = l - 1; i >= 0; i--)
        if (s[o + i] == '.') return i > off ? i : l;
    return l;
}
static int tvp_single(const uint8_t *s, int64_t o, int64_t l, int64_t i, int cap) {
    int prev = i == 0 ? TVP_DELIM : tvp_cls(s[o + i - 1]);
    int score = 16 + tvp_bonus(prev, tvp_cls(s[o + i])) * 2;
    int64_t off = (l > 1 && s[o] == '.') ? 1 : 0;
    if (i == off) {
        int64_t stem = tvp_stem(s, o, l, off);
        int placed = i + 1 == l ? 100 : i + 1 == stem ? 80 : 30;
        score += cap < placed ? cap : placed;
    }
    return score - tvp_lenpen(l);
}
static int tvp_fuzzy(const uint8_t *s, int64_t o, int64_t l, const uint8_t *q, int64_t ql, int cap) {
    int64_t end = 0, from = 0;
    for (int64_t k = 0; k < ql; k++) {
        int64_t f = find_folded(s, o + from, o + l, q[k]);
        if (f < 0) return TVP_NO;
        end = f - o;
        from = end + 1;
    }
    if (ql == 1) return tvp_single(s, o, l, end, cap);
    int64_t start = end + 1;
    for (int64_t k = ql - 1; k >= 0; k--) {
        int64_t f = rfind_folded(s, o, o + start, q[k]);
        if (f < 0) return TVP_NO;
        start = f - o;
    }
    int score = 0, first = 0;
    int64_t at = start;
    for (int64_t k = 0; k < ql; k++) {
        int run = 0;
        if (k > 0) {
            int64_t last = at;
            int64_t f = find_folded(s, o + last + 1, o + end + 1, q[k]);
            if (f < 0) return TVP_NO;
            at = f - o;
            run = at == last + 1;
            if (!run) score += -3 - (int)(at - last - 2);
        }
        int prev = at == 0 ? TVP_DELIM : tvp_cls(s[o + at - 1]);
        int b = tvp_bonus(prev, tvp_cls(s[o + at]));
        if (run) {
            if (b >= 8 && b > first) first = b;
            if (first > b) b = first;
            if (b < 4) b = 4;
        } else {
            first = b;
        }
        score += 16 + (k == 0 ? b * 2 : b);
    }
    int64_t off = (l > 1 && s[o] == '.') ? 1 : 0;
    int64_t stem = tvp_stem(s, o, l, off);
    int contiguous = end + 1 - start == ql;
    int placed = 0;
    if (start == off && contiguous && end + 1 == l) placed = 100;
    else if (start == off && contiguous && end + 1 == stem) placed = 80;
    else if (start == off && contiguous) placed = 30;
    return score + (cap < placed ? cap : placed) - tvp_lenpen(l);
}
static inline int tvp_digit(int b) { return b >= '0' && b <= '9'; }
static inline int tvp_starts(const uint8_t *s, int64_t wo, int64_t wl, const uint8_t *q, int64_t ql, int64_t qf) {
    int64_t need = ql - qf;
    if (wl < need) return 0;
    for (int64_t j = 0; j < need; j++)
        if (tvp_fold(s[wo + j]) != q[qf + j]) return 0;
    return 1;
}
static int64_t tvp_one_edit(const uint8_t *s, int64_t wo, int64_t wl, const uint8_t *q, int64_t ql) {
    int64_t i = -1;
    for (int64_t k = 0; k < ql; k++)
        if (k >= wl || tvp_fold(s[wo + k]) != q[k]) { i = k; break; }
    if (i < 0) return TVP_NO;
    if (tvp_digit(q[i]) || (i < wl && tvp_digit(s[wo + i]))) return TVP_NO;
    if (i + 1 < ql && i + 1 < wl && tvp_fold(s[wo + i]) == q[i + 1] && tvp_fold(s[wo + i + 1]) == q[i] && tvp_starts(s, wo + i + 2, wl - i - 2, q, ql, i + 2)) return ql;
    if (i < wl && tvp_starts(s, wo + i + 1, wl - i - 1, q, ql, i + 1)) return ql;
    if (tvp_starts(s, wo + i, wl - i, q, ql, i + 1)) return ql - 1;
    if (i < wl && tvp_starts(s, wo + i + 1, wl - i - 1, q, ql, i)) return ql + 1;
    return TVP_NO;
}
static int tvp_typo_at(const uint8_t *s, int64_t o, int64_t l, int64_t st, const uint8_t *q, int64_t ql) {
    if (st >= l || tvp_fold(s[o + st]) != q[0]) return TVP_NO;
    int64_t len = tvp_one_edit(s, o + st, l - st, q, ql);
    if (len == TVP_NO || len > 128) return TVP_NO;
    uint8_t fixed[128];
    for (int64_t j = 0; j < len; j++) fixed[j] = (uint8_t)tvp_fold(s[o + st + j]);
    int r = tvp_fuzzy(s, o, l, fixed, len, 30);
    return r == TVP_NO ? TVP_NO : r - 60;
}
static int tvp_typo(const uint8_t *s, int64_t o, int64_t l, uint64_t m, const uint8_t *q, int64_t ql) {
    int best = tvp_typo_at(s, o, l, (l > 1 && s[o] == '.') ? 1 : 0, q, ql);
    if (m & (UINT64_C(1) << 38))
        for (int64_t j = 0; j < l; j++)
            if (s[o + j] == ' ') {
                int r = tvp_typo_at(s, o, l, j + 1, q, ql);
                if (r > best) best = r;
            }
    return best;
}
static inline int tvp_fits(uint64_t m, uint64_t need, uint64_t loose, uint64_t start) {
    uint64_t miss = need & ~m;
    if (miss == 0) return 1;
    return ((miss & ~loose) | (miss & (miss - 1))) == 0 && (m & start) != 0;
}

typedef struct {
    const uint8_t *s, *tText, *xText;
    const uint32_t *uoff, *cand, *neOff, *tOff, *xOff;
    const uint64_t *mask, *tMask, *tLoose, *tStart;
    const int64_t *tMode;
    int64_t np, nt, nx, count, chunk;
    bool fromCand;
    uint32_t gen;
    uint32_t *hStamp;
    int32_t *hScore, *hB0, *hB1, *hB2, *hB3;
    uint8_t *hBits, *hFlags;
    uint32_t **ok, **pres;
    int64_t *nok, *okEnt, *npres;
} tvp_names;

static int tvp_token(const tvp_names *x, int64_t o, int64_t l, uint64_t m, int64_t t) {
    const uint8_t *q = x->tText + x->tOff[t];
    int64_t ql = x->tOff[t + 1] - x->tOff[t];
    switch (x->tMode[t]) {
    case 0: /* fuzzy */
        if (ql >= 5) {
            int clean = (x->tMask[t] & ~m) == 0 ? tvp_fuzzy(x->s, o, l, q, ql, 100) : TVP_NO;
            int typo = (m & x->tStart[t]) != 0 ? tvp_typo(x->s, o, l, m, q, ql) : TVP_NO;
            return clean > typo ? clean : typo;
        }
        return tvp_fuzzy(x->s, o, l, q, ql, 100);
    case 1: /* exact */
        for (int64_t i = 0; i + ql <= l; i++)
            if (tvp_starts(x->s, o + i, ql, q, ql, 0)) return 40 + (i == 0 ? 30 : 0) - tvp_lenpen(l);
        return TVP_NO;
    case 2: /* prefix */
        return tvp_starts(x->s, o, l, q, ql, 0) ? 60 - tvp_lenpen(l) : TVP_NO;
    default: /* suffix */
        return l >= ql && tvp_starts(x->s, o + l - ql, ql, q, ql, 0) ? 50 - tvp_lenpen(l) : TVP_NO;
    }
}
static int tvp_matches(const tvp_names *x, int64_t o, int64_t l, int64_t t) {
    if (x->tMode[t] == 0) {
        const uint8_t *q = x->tText + x->tOff[t];
        int64_t ql = x->tOff[t + 1] - x->tOff[t], j = 0;
        for (int64_t i = 0; i < l; i++)
            if (tvp_fold(x->s[o + i]) == q[j] && ++j == ql) return 1;
        return 0;
    }
    return tvp_token(x, o, l, ~UINT64_C(0), t) != TVP_NO;
}
static int tvp_ext_ok(const tvp_names *x, int64_t o, int64_t l) {
    /* A vector scan, not a byte loop: the byte loop's speed swung by 20%
     * with where unrelated code changes happened to place it. */
    int64_t dot = rfind_folded(x->s, o, o + l, '.');
    if (dot < 0) return 0;
    int64_t eo = dot + 1, el = o + l - dot - 1;
    for (int64_t e = 0; e < x->nx; e++) {
        int64_t xo = x->xOff[e];
        if ((int64_t)x->xOff[e + 1] - xo != el) continue;
        int64_t j = 0;
        while (j < el && x->xText[xo + j] == tvp_fold(x->s[eo + j])) j++;
        if (j == el) return 1;
    }
    return 0;
}
static inline int16_t tvp_clamp16(int v) { return (int16_t)(v < -32768 ? -32768 : v > 32767 ? 32767 : v); }

static void tvp_names_chunk(void *ctx, size_t ci) {
    const tvp_names *x = ctx;
    int64_t a = (int64_t)ci * x->chunk, b = a + x->chunk < x->count ? a + x->chunk : x->count;
    uint32_t *ok = x->ok[ci], *pres = x->pres[ci];
    int64_t nok = 0, okEnt = 0, npres = 0;
    for (int64_t c = a; c < b; c++) {
        int64_t k = x->fromCand ? (int64_t)x->cand[c] : c;
        uint64_t m = x->mask[k];
        int64_t o = x->uoff[k], l = (int64_t)x->uoff[k + 1] - o;
        int score = 0, bits = 0, flags = 0;
        int best[4] = {0, 0, 0, 0};
        if (l > 0 && x->s[o] == '.') flags |= 2;
        if (l >= 4 && memcmp(x->s + o + l - 4, ".app", 4) == 0) flags |= 4;
        for (int64_t t = 0; t < x->np; t++) {
            if (!tvp_fits(m, x->tMask[t], x->tLoose[t], x->tStart[t])) continue;
            int sc = tvp_token(x, o, l, m, t);
            if (sc == TVP_NO) continue;
            bits |= 1 << t;
            int s16 = tvp_clamp16(sc);
            score = tvp_clamp16(score + s16);
            if (t < 4) best[t] = s16 > 0 ? s16 : 0;
        }
        for (int64_t t = x->np; t < x->nt; t++)
            if (tvp_matches(x, o, l, t)) { flags |= 8; break; }
        int okk = (x->np == 0 || bits != 0) && (flags & 8) == 0 && (x->nx == 0 || tvp_ext_ok(x, o, l));
        if (okk) {
            flags |= 1;
            okEnt += (int64_t)x->neOff[k + 1] - x->neOff[k];
            ok[nok++] = (uint32_t)k;
        }
        if (okk || bits || (flags & 8)) {
            x->hStamp[k] = x->gen;
            x->hScore[k] = score;
            x->hBits[k] = (uint8_t)bits;
            x->hFlags[k] = (uint8_t)flags;
            x->hB0[k] = best[0]; x->hB1[k] = best[1]; x->hB2[k] = best[2]; x->hB3[k] = best[3];
            pres[npres++] = (uint32_t)k;
        }
    }
    x->nok[ci] = nok;
    x->okEnt[ci] = okEnt;
    x->npres[ci] = npres;
}
int64_t k_scoreNamesLoop(const k_scoreNamesLoop_args *a) {
    tvp_names x = {
        .s = a->unames, .tText = a->tText, .xText = a->xText,
        .uoff = a->uoff, .cand = a->cand, .neOff = a->neOff,
        .tOff = a->tOff, .xOff = a->xOff,
        .mask = a->umask, .tMask = a->tMask, .tLoose = a->tLoose, .tStart = a->tStart,
        .tMode = a->tMode,
        .np = a->qNpos, .nt = a->qNpos + a->qNneg, .nx = a->xOff_len - 1, .count = a->count, .chunk = 1 << 14,
        .fromCand = a->fromCand,
        .gen = (uint32_t)a->gen,
        .hStamp = a->hStamp, .hScore = a->hScore,
        .hB0 = a->hBest0, .hB1 = a->hBest1, .hB2 = a->hBest2, .hB3 = a->hBest3,
        .hBits = a->hBits, .hFlags = a->hFlags,
    };
    size_t chunks = (size_t)((a->count + x.chunk - 1) / x.chunk);
    if (chunks == 0) { *a->okEntries = 0; *a->presCount = 0; return 0; }
    x.ok = malloc(chunks * sizeof *x.ok);
    x.nok = calloc(chunks, sizeof *x.nok);
    x.okEnt = calloc(chunks, sizeof *x.okEnt);
    x.pres = malloc(chunks * sizeof *x.pres);
    x.npres = calloc(chunks, sizeof *x.npres);
    for (size_t c = 0; c < chunks; c++) {
        x.ok[c] = malloc((size_t)x.chunk * sizeof(uint32_t));
        x.pres[c] = malloc((size_t)x.chunk * sizeof(uint32_t));
    }
    par_for(chunks, &x, tvp_names_chunk);
    uint32_t *out = a->okBuf, *pout = a->presBuf;
    int64_t nok = 0, ent = 0, npres = 0;
    for (size_t c = 0; c < chunks; c++) {
        memcpy(out + nok, x.ok[c], (size_t)x.nok[c] * sizeof(uint32_t));
        memcpy(pout + npres, x.pres[c], (size_t)x.npres[c] * sizeof(uint32_t));
        nok += x.nok[c];
        npres += x.npres[c];
        ent += x.okEnt[c];
        free(x.ok[c]);
        free(x.pres[c]);
    }
    free(x.ok); free(x.nok); free(x.okEnt); free(x.pres); free(x.npres);
    *a->okEntries = ent;
    *a->presCount = npres;
    return nok;
}
