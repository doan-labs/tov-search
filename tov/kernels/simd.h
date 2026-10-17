/* The SIMD the kernels share, and the only CPU-specific code in them. Vectors
 * are GCC/Clang vector extensions, so one source compiles to NEON, SSE2 or
 * AVX2 or AVX-512; only turning a byte compare into a bit mask needs an intrinsic.
 *
 *   AVX-512BW (x86 with -mavx512bw or -march=native on one)  64-byte vectors
 *   AVX2 (x86 with -mavx2 or -march=native on a recent CPU)  32-byte vectors
 *   SSE2 (every x86-64)                                      16-byte vectors
 *   NEON (every arm64)                                       16-byte vectors
 *   anything else                                            16-byte vectors, a scalar mask
 *
 * Loads read a whole vector from any name: engine.tov pads unames with 64 bytes. */
#pragma once
#include <stdint.h>
#include <string.h>

#if defined(__AVX512BW__)
#include <immintrin.h>
#define VW 64
#define LB 1
#elif defined(__AVX2__)
#include <immintrin.h>
#define VW 32
#define LB 1 /* bits per byte lane in bytemask() */
#elif defined(__SSE2__)
#include <emmintrin.h>
#define VW 16
#define LB 1
#elif defined(__ARM_NEON)
#include <arm_neon.h>
#define VW 16
#define LB 4
#else
#define VW 16
#define LB 1
#endif
#define U64_LANES (VW / 8)

typedef uint8_t u8v __attribute__((vector_size(VW)));
typedef uint64_t u64v __attribute__((vector_size(VW)));

static inline u8v load8(const uint8_t *p) { u8v v; memcpy(&v, p, sizeof v); return v; }
static inline u8v splat8(uint8_t x) { u8v v; memset(&v, x, sizeof v); return v; }
static inline u64v splat64(uint64_t x) { return (u64v){0} + x; }

/* LB bits per lane of a byte compare (lanes all-ones or zero), lane 0 lowest. */
static inline uint64_t bytemask(u8v eq) {
#if defined(__AVX512BW__)
    return _mm512_movepi8_mask((__m512i)eq);
#elif defined(__AVX2__)
    return (uint32_t)_mm256_movemask_epi8((__m256i)eq);
#elif defined(__SSE2__)
    return (uint32_t)_mm_movemask_epi8((__m128i)eq);
#elif defined(__ARM_NEON)
    /* The vshrn trick: narrow each 16-bit pair to one byte, 4 bits per lane. */
    return vget_lane_u64(vreinterpret_u64_u8(vshrn_n_u16(vreinterpretq_u16_u8((uint8x16_t)eq), 4)), 0);
#else
    uint64_t m = 0;
    for (int i = 0; i < VW; i++) m |= (uint64_t)(eq[i] != 0) << i;
    return m;
#endif
}

/* Lanes of s[i, i + VW) that fold to c (c already lowercase), cut off at `to`. */
static inline uint64_t folded_hits(const uint8_t *s, int64_t i, int64_t to, uint8_t c) {
    uint8_t up = (c >= 'a' && c <= 'z') ? (uint8_t)(c - 32) : c;
    u8v b = load8(s + i);
    uint64_t m = bytemask((u8v)((b == splat8(c)) | (b == splat8(up))));
    int64_t rem = to - i;
    return rem < VW ? m & ((UINT64_C(1) << (rem * LB)) - 1) : m;
}

/* First / last index in s[from, to) whose byte folds to c, or -1. */
static inline int64_t find_folded(const uint8_t *s, int64_t from, int64_t to, uint8_t c) {
    for (int64_t i = from; i < to; i += VW) {
        uint64_t m = folded_hits(s, i, to, c);
        if (m) return i + __builtin_ctzll(m) / LB;
    }
    return -1;
}
static inline int64_t rfind_folded(const uint8_t *s, int64_t from, int64_t to, uint8_t c) {
    /* Names are short: scan forward and keep the last hit. */
    int64_t last = -1;
    for (int64_t i = from; i < to; i += VW) {
        uint64_t m = folded_hits(s, i, to, c);
        if (m) last = i + (63 - __builtin_clzll(m)) / LB;
    }
    return last;
}

/* Token.fits: name mask m has every char class the token needs, or misses one
 * loose class (a typo) and has a word starting with the token's first letter. */
static inline int fits1(uint64_t m, uint64_t need, uint64_t loose, uint64_t start) {
    uint64_t miss = need & ~m;
    return !miss || (!((miss & ~loose) | (miss & (miss - 1))) && (m & start));
}
static inline u64v fitsv(u64v m, u64v need, u64v loose, u64v start) {
    u64v miss = need & ~m;
    u64v rest = (miss & ~loose) | (miss & (miss - 1));
    return (u64v)((miss == 0) | ((rest == 0) & ((m & start) != 0)));
}

/* Ids k in [lo, hi) whose mask fits any of the nt tokens, ascending, into out;
 * returns how many. Branchless: every id is written, then kept or overwritten,
 * so out must hold hi - lo ids. */
static inline int64_t prefilter_range(const uint64_t *mask, int64_t lo, int64_t hi, int64_t nt,
                                      const uint64_t *need, const uint64_t *loose, const uint64_t *start,
                                      uint32_t *out) {
    int64_t c = 0, k = lo;
    if (nt <= 16) { /* else more tokens than registers: one name at a time below */
        u64v vn[16], vl[16], vs[16];
        for (int64_t t = 0; t < nt; t++) {
            vn[t] = splat64(need[t]);
            vl[t] = splat64(loose[t]);
            vs[t] = splat64(start[t]);
        }
        for (; k + U64_LANES <= hi; k += U64_LANES) {
            u64v m, acc = {0};
            memcpy(&m, mask + k, sizeof m);
            for (int64_t t = 0; t < nt; t++) acc |= fitsv(m, vn[t], vl[t], vs[t]);
            for (int j = 0; j < U64_LANES; j++) {
                out[c] = (uint32_t)(k + j);
                c += (int64_t)(acc[j] & 1);
            }
        }
    }
    for (; k < hi; k++)
        for (int64_t t = 0; t < nt; t++)
            if (fits1(mask[k], need[t], loose[t], start[t])) {
                out[c++] = (uint32_t)k;
                break;
            }
    return c;
}
