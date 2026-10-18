/* A file's grams for the content index, as src/cgrams.tov's grams(): sorted
 * distinct folded trigrams (plus the end-of-file pair key), sorted distinct
 * definition keys, and the distinct 5- and 7-byte gram hashes, over the file
 * and its odd folds (ſ and the Kelvin sign read as s and k). Shared by the
 * gramCount and gramFill kernels. */
#pragma once
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    uint32_t *tri, *sym, *sh, *lg;
    size_t ntri, nsym, nsh, nlg;
} gr_doc;

static inline uint32_t gr_fold(uint8_t b) { return (uint32_t)(b | (((uint8_t)(b - 'A') < 26) << 5)); }

static inline uint32_t gr_hash(uint64_t g) { return (uint32_t)((g * 0x9E3779B97F4A7C15ULL) >> 40); }

static inline size_t gr_bit(uint32_t h, size_t words, int second) {
    uint32_t x = second ? (h * 0x9E3779B1u) & 0xFFFFFFu : h;
    return (size_t)(((uint64_t)x * words * 64) >> 24);
}

/* One 2^24-bit seen set per thread, all zero between uses. */
static __thread uint64_t *gr_seen;

typedef struct {
    uint32_t *v;
    size_t n, cap;
} gr_list;

static void gr_add(gr_list *l, uint32_t x) {
    uint64_t bit = 1ULL << (x & 63);
    if (gr_seen[x >> 6] & bit) return;
    gr_seen[x >> 6] |= bit;
    if (l->n == l->cap) {
        l->cap = l->cap ? l->cap * 2 : 1024;
        l->v = realloc(l->v, l->cap * sizeof *l->v);
    }
    l->v[l->n++] = x;
}

static void gr_unsee(const gr_list *l) {
    for (size_t i = 0; i < l->n; i++) gr_seen[l->v[i] >> 6] = 0;
}

static int gr_cmp(const void *a, const void *b) {
    uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b;
    return x < y ? -1 : x > y;
}

static void gr_trigrams(const uint8_t *b, size_t n, gr_list *l) {
    if (n >= 2) gr_add(l, (gr_fold(b[n - 2]) << 16) | (gr_fold(b[n - 1]) << 8));
    if (n < 3) return;
    uint32_t t = (gr_fold(b[0]) << 8) | gr_fold(b[1]);
    for (size_t i = 2; i < n; i++) {
        t = ((t << 8) | gr_fold(b[i])) & 0xFFFFFFu;
        gr_add(l, t);
    }
}

static void gr_hashes(const uint8_t *b, size_t n, size_t w, gr_list *l) {
    if (n < w) return;
    uint64_t g = 0, mask = (1ULL << (8 * w)) - 1;
    for (size_t i = 0; i < n; i++) {
        g = ((g << 8) | gr_fold(b[i])) & mask;
        if (i + 1 >= w) gr_add(l, gr_hash(g));
    }
}

static const char *const gr_defines[] = {"fn", "func", "function", "def", "class", "struct", "enum", "trait", "interface", "type",
    "typealias", "impl", "let", "const", "var", "val", "static", "module", "mod", "protocol", "extension", "macro_rules!",
    "define", "typedef", "union", "object", "record", "namespace", "actor"};

static inline int gr_word(uint8_t b) { return (b >= '0' && b <= '9') || (b >= 'A' && b <= 'Z') || (b >= 'a' && b <= 'z') || b == '_'; }
static inline int gr_istart(uint8_t b) { return (b >= 'A' && b <= 'Z') || (b >= 'a' && b <= 'z') || b == '_'; }
static inline int gr_sep(uint8_t b) { return b == ' ' || b == '\t' || b == '*' || b == '&'; }
static inline int gr_blank(uint8_t b) { return b == ' ' || b == '\t'; }

/* Where the name of a definition whose keyword starts at p begins, or end. */
static size_t gr_def_name(const uint8_t *b, size_t p, size_t end) {
    for (size_t k = 0; k < sizeof gr_defines / sizeof *gr_defines; k++) {
        size_t kl = strlen(gr_defines[k]);
        if (p + kl > end || memcmp(b + p, gr_defines[k], kl) != 0) continue;
        size_t q = p + kl;
        if (q < end && b[q] == '<') {
            size_t r = q + 1;
            while (r < end && b[r] != '>' && b[r] != '\n') r++;
            if (r >= end || b[r] != '>') continue;
            q = r + 1;
        }
        size_t j = q;
        while (j < end && gr_sep(b[j])) j++;
        if (j == q) continue;
        if (j + 3 < end && b[j] == 'm' && b[j + 1] == 'u' && b[j + 2] == 't' && gr_blank(b[j + 3])) {
            size_t m = j + 3;
            while (m < end && gr_blank(b[m])) m++;
            if (m < end && gr_istart(b[m])) return m;
        }
        if (j < end && gr_istart(b[j])) return j;
    }
    return end;
}

static void gr_symbols(const uint8_t *b, size_t n, gr_list *l) {
    size_t p = 0;
    while (p < n) {
        if (gr_istart(b[p]) && (p == 0 || !gr_word(b[p - 1]))) {
            size_t at = gr_def_name(b, p, n);
            if (at < n) {
                size_t e = at;
                uint32_t h = 2166136261u;
                while (e < n && gr_word(b[e])) h = (h ^ b[e++]) * 16777619u;
                if (l->n == l->cap) {
                    l->cap = l->cap ? l->cap * 2 : 64;
                    l->v = realloc(l->v, l->cap * sizeof *l->v);
                }
                l->v[l->n++] = h | 0x80000000u;
                p = at;
                continue;
            }
        }
        p++;
    }
    qsort(l->v, l->n, sizeof *l->v, gr_cmp);
    size_t w = 0;
    for (size_t i = 0; i < l->n; i++)
        if (w == 0 || l->v[w - 1] != l->v[i]) l->v[w++] = l->v[i];
    l->n = w;
}

/* b with ſ and the Kelvin sign as s and k, or NULL if it holds neither. */
static uint8_t *gr_odd(const uint8_t *b, size_t n, size_t *out_n) {
    int any = 0;
    for (size_t i = 0; i + 1 < n && !any; i++)
        any = (b[i] == 0xC5 && b[i + 1] == 0xBF) || (b[i] == 0xE2 && i + 2 < n && b[i + 1] == 0x84 && b[i + 2] == 0xAA);
    if (!any) return NULL;
    uint8_t *o = malloc(n);
    size_t w = 0;
    for (size_t i = 0; i < n;) {
        if (b[i] == 0xC5 && i + 1 < n && b[i + 1] == 0xBF) {
            o[w++] = 's';
            i += 2;
        } else if (b[i] == 0xE2 && i + 2 < n && b[i + 1] == 0x84 && b[i + 2] == 0xAA) {
            o[w++] = 'k';
            i += 3;
        } else {
            o[w++] = b[i++];
        }
    }
    *out_n = w;
    return o;
}

/* The grams of b[0, n); free the lists with gr_free. */
static gr_doc gr_grams(const uint8_t *b, size_t n) {
    if (!gr_seen) gr_seen = calloc((1u << 24) / 64, sizeof *gr_seen);
    size_t an = 0;
    uint8_t *alt = gr_odd(b, n, &an);
    gr_list tri = {0}, sym = {0}, sh = {0}, lg = {0};
    gr_trigrams(b, n, &tri);
    if (alt) gr_trigrams(alt, an, &tri);
    gr_unsee(&tri);
    qsort(tri.v, tri.n, sizeof *tri.v, gr_cmp);
    gr_hashes(b, n, 5, &sh);
    if (alt) gr_hashes(alt, an, 5, &sh);
    gr_unsee(&sh);
    gr_hashes(b, n, 7, &lg);
    if (alt) gr_hashes(alt, an, 7, &lg);
    gr_unsee(&lg);
    gr_symbols(b, n, &sym);
    free(alt);
    return (gr_doc){tri.v, sym.v, sh.v, lg.v, tri.n, sym.n, sh.n, lg.n};
}

static void gr_free(gr_doc *d) {
    free(d->tri);
    free(d->sym);
    free(d->sh);
    free(d->lg);
}
