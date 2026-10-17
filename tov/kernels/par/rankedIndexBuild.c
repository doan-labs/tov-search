#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include "par.h"

#define RI_CHUNK 16384
#define RI_WANT 16384
#define RI_START_WORDS 4096

typedef struct {
    const k_rankedIndexBuild_args *a;
    int64_t step;
} ri_name_ctx;

static int ri_lower(int b) {
    return b >= 'A' && b <= 'Z' ? b + ('a' - 'A') : b;
}

static void ri_names(void *ctx, size_t ci) {
    const ri_name_ctx *x = ctx;
    const k_rankedIndexBuild_args *a = x->a;
    int64_t lo = (int64_t)ci * x->step;
    int64_t hi = lo + x->step < a->u ? lo + x->step : a->u;
    for (int64_t id = lo; id < hi; id++) {
        uint32_t begin = a->neOff[id], end = a->neOff[id + 1];
        int32_t prior = 0;
        if (begin < end) {
            uint32_t e = a->ne[begin];
            prior = a->dirPrior[a->entParent[e]];
            for (uint32_t j = begin + 1; j < end; j++) {
                e = a->ne[j];
                int32_t p = a->dirPrior[a->entParent[e]];
                if (p > prior) prior = p;
            }
        }
        a->rankedNamePrior[id] = (int8_t)prior;
        uint32_t o = a->uoff[id], end_o = a->uoff[id + 1];
        int len = (int)(end_o - o);
        int off = len > 1 && a->unames[o] == '.' ? 1 : 0;
        int stem = len;
        for (int j = len - 1; j >= 0; j--) {
            if (a->unames[o + (uint32_t)j] == '.') {
                if (j > off) stem = j;
                break;
            }
        }
        int len8 = len - off > 255 ? 255 : len - off;
        int stem8 = stem - off > 255 ? 255 : stem - off;
        int head = off < len ? ri_lower(a->unames[o + (uint32_t)off]) : 0;
        int tweak = 10;
        if (len >= 4 && a->unames[end_o - 4] == '.' &&
            a->unames[end_o - 3] == 'a' && a->unames[end_o - 2] == 'p' &&
            a->unames[end_o - 1] == 'p')
            tweak += 25;
        if (len > 0 && a->unames[o] == '.') tweak -= 8;
        int penalty = (len < 80 ? len : 80) / 3;
        int key = (int)prior + tweak - penalty;
        if (key < -128) key = -128;
        uint32_t info = (uint8_t)key | ((uint32_t)len8 << 8) |
                        ((uint32_t)stem8 << 16) | ((uint32_t)head << 24);
        a->rankedNameInfo[id] = info;
    }
}

typedef struct {
    const k_rankedIndexBuild_args *a;
    uint32_t *key_counts;
    uint32_t *short_counts;
    uint32_t *class_counts;
    int64_t step;
} ri_hist_ctx;

static void ri_hist(void *ctx, size_t ci) {
    const ri_hist_ctx *x = ctx;
    const k_rankedIndexBuild_args *a = x->a;
    int64_t lo = (int64_t)ci * x->step;
    int64_t hi = lo + x->step < a->u ? lo + x->step : a->u;
    uint32_t *keys = x->key_counts + ci * 256;
    uint32_t *shorts = x->short_counts + ci * 256;
    uint32_t *classes = x->class_counts + ci * 41;
    for (int64_t id = lo; id < hi; id++) {
        uint32_t info = a->rankedNameInfo[id];
        uint32_t key = ((info & 255) + 128) & 255;
        uint32_t len = (info >> 8) & 255;
        uint32_t stem = (info >> 16) & 255;
        uint32_t head = info >> 24;
        keys[key]++;
        if (len <= 3 || stem <= 3) shorts[head]++;
    }
    for (int64_t base = lo; base < hi; base += 64) {
        uint64_t planes[41] = {0};
        int64_t end = base + 64 < hi ? base + 64 : hi;
        for (int64_t id = base; id < end; id++) {
            uint64_t m = a->umask[id] & ((UINT64_C(1) << 41) - 1);
            while (m) {
                unsigned bit = (unsigned)__builtin_ctzll(m);
                planes[bit] |= UINT64_C(1) << (unsigned)(id - base);
                m &= m - 1;
            }
        }
        for (unsigned bit = 0; bit < 41; bit++)
            classes[bit] += (uint32_t)__builtin_popcountll(planes[bit]);
    }
}

typedef struct {
    const k_rankedIndexBuild_args *a;
    const uint32_t *key_offsets;
    const uint32_t *short_offsets;
    size_t chunks;
    int64_t step;
    int cover_index;
} ri_scatter_ctx;

static void ri_scatter(void *ctx, size_t ci) {
    const ri_scatter_ctx *x = ctx;
    const k_rankedIndexBuild_args *a = x->a;
    int64_t lo = (int64_t)ci * x->step;
    int64_t hi = lo + x->step < a->u ? lo + x->step : a->u;
    uint32_t key_used[256] = {0};
    uint32_t short_used[256] = {0};
    for (int64_t id = lo; id < hi; id++) {
        uint32_t info = a->rankedNameInfo[id];
        uint32_t key = ((info & 255) + 128) & 255;
        if ((int)key >= x->cover_index) {
            uint32_t at = x->key_offsets[ci * 256 + key] + key_used[key]++;
            a->rankedByList[at] = (uint32_t)id;
        }
        uint32_t len = (info >> 8) & 255;
        uint32_t stem = (info >> 16) & 255;
        if (len <= 3 || stem <= 3) {
            uint32_t head = info >> 24;
            uint32_t at = x->short_offsets[ci * 256 + head] + short_used[head]++;
            a->rankedShortIds[at] = (uint32_t)id;
        }
    }
}

typedef struct {
    const k_rankedIndexBuild_args *a;
    size_t nw;
    size_t blocks;
} ri_start_ctx;

static void ri_start(void *ctx, size_t task) {
    const ri_start_ctx *x = ctx;
    const k_rankedIndexBuild_args *a = x->a;
    size_t row = task / x->blocks;
    size_t block = task % x->blocks;
    size_t lo = block * RI_START_WORDS;
    size_t hi = lo + RI_START_WORDS < x->nw ? lo + RI_START_WORDS : x->nw;
    for (size_t w = lo; w < hi; w++) {
        uint64_t bits = 0;
        size_t first = w * 64;
        size_t end = first + 64 < (size_t)a->u ? first + 64 : (size_t)a->u;
        for (size_t id = first; id < end; id++)
            if ((a->umask[id] >> (41 + row)) & 1)
                bits |= UINT64_C(1) << (id & 63);
        a->rankedStartFit[row * x->nw + w] = bits;
    }
}

void k_rankedIndexBuild(const k_rankedIndexBuild_args *a) {
    size_t chunks = (size_t)(a->u + RI_CHUNK - 1) / RI_CHUNK;
    if (!chunks) chunks = 1;
    ri_name_ctx names = {.a = a, .step = (a->u + (int64_t)chunks - 1) / (int64_t)chunks};
    par_for(chunks, &names, ri_names);

    uint32_t *key_hist = calloc(chunks * 256, sizeof *key_hist);
    uint32_t *short_hist = calloc(chunks * 256, sizeof *short_hist);
    uint32_t *class_hist = calloc(chunks * 41, sizeof *class_hist);
    if (!key_hist || !short_hist || !class_hist) abort();
    ri_hist_ctx hist = {
        .a = a, .key_counts = key_hist, .short_counts = short_hist,
        .class_counts = class_hist, .step = names.step,
    };
    par_for(chunks, &hist, ri_hist);

    memset((void *)a->rankedKeyCounts, 0, 256 * sizeof(uint32_t));
    uint32_t short_counts[256] = {0};
    for (size_t c = 0; c < chunks; c++) {
        for (size_t k = 0; k < 256; k++)
            a->rankedKeyCounts[k] += key_hist[c * 256 + k];
        for (size_t h = 0; h < 256; h++)
            short_counts[h] += short_hist[c * 256 + h];
        for (size_t b = 0; b < 41; b++)
            a->rankedClassCounts[b] += class_hist[c * 41 + b];
    }
    int cover = 128;
    uint32_t selected = 0;
    while (cover > -128) {
        uint32_t add = a->rankedKeyCounts[cover + 127];
        if ((uint64_t)selected + add > RI_WANT) break;
        cover--;
        selected += a->rankedKeyCounts[cover + 128];
    }
    *a->rankedCover = cover;
    *a->rankedByListCount = selected;

    uint32_t key_base[256] = {0};
    uint32_t at = 0;
    for (int k = 255; k >= cover + 128; k--) {
        key_base[k] = at;
        at += a->rankedKeyCounts[k];
    }
    for (int k = 255; k >= cover + 128; k--) {
        uint32_t offset = key_base[k];
        for (size_t c = 0; c < chunks; c++) {
            uint32_t count = key_hist[c * 256 + (size_t)k];
            key_hist[c * 256 + (size_t)k] = offset;
            offset += count;
        }
    }
    for (size_t h = 0; h < 256; h++)
        a->rankedShortOff[h + 1] = a->rankedShortOff[h] + short_counts[h];
    for (size_t h = 0; h < 256; h++) {
        uint32_t offset = a->rankedShortOff[h];
        for (size_t c = 0; c < chunks; c++) {
            uint32_t count = short_hist[c * 256 + h];
            short_hist[c * 256 + h] = offset;
            offset += count;
        }
    }
    ri_scatter_ctx scatter = {
        .a = a, .key_offsets = key_hist, .short_offsets = short_hist,
        .chunks = chunks, .step = names.step, .cover_index = cover + 128,
    };
    par_for(chunks, &scatter, ri_scatter);

    uint32_t head_counts[256] = {0}, head_next[256] = {0};
    for (uint32_t p = 0; p < selected; p++)
        head_counts[a->rankedNameInfo[a->rankedByList[p]] >> 24]++;
    for (size_t h = 0; h < 256; h++) {
        a->rankedHeadOff[h + 1] = a->rankedHeadOff[h] + head_counts[h];
        head_next[h] = a->rankedHeadOff[h];
    }
    for (uint32_t p = 0; p < selected; p++) {
        uint32_t id = a->rankedByList[p];
        uint32_t head = a->rankedNameInfo[id] >> 24;
        a->rankedHeadIds[head_next[head]++] = id;
    }

    free(class_hist);
    free(short_hist);
    free(key_hist);

    size_t nw = ((size_t)a->u + 63) / 64;
    size_t blocks = (nw + RI_START_WORDS - 1) / RI_START_WORDS;
    if (blocks) {
        ri_start_ctx starts = {.a = a, .nw = nw, .blocks = blocks};
        par_for(23 * blocks, &starts, ri_start);
    }
}
