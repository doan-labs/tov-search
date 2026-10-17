#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>

#include "hooks.h"

static int64_t trailing_zeros_reference(uint64_t bits) {
    int64_t count = 0;
    if (bits == UINT64_C(0)) {
        return 64;
    }
    while ((bits & UINT64_C(1)) == UINT64_C(0)) {
        bits >>= 1;
        count++;
    }
    return count;
}

static int check(uint64_t bits, int64_t expected, const char *label) {
    const k_trailingZeros64_args args = {.bits = bits};
    const int64_t actual = k_trailingZeros64(&args);
    if (actual != expected) {
        fprintf(stderr,
                "%s: bits=0x%016" PRIx64 " expected=%" PRId64 " actual=%" PRId64 "\n",
                label, bits, expected, actual);
        return 1;
    }
    return 0;
}

static int check_reference(uint64_t bits, const char *label) {
    return check(bits, trailing_zeros_reference(bits), label);
}

static uint64_t next_random(uint64_t *state) {
    uint64_t value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    return value;
}

int main(void) {
    uint64_t cases = 0;
    uint64_t random_state = UINT64_C(0x9e3779b97f4a7c15);

    if (check(UINT64_C(0), 64, "zero")) {
        return 1;
    }
    cases++;
    for (unsigned int bit = 0; bit < 64; bit++) {
        if (check(UINT64_C(1) << bit, (int64_t)bit, "isolated bit")) {
            return 1;
        }
        cases++;
    }
    if (check(UINT64_MAX, 0, "all ones")) {
        return 1;
    }
    cases++;
    for (unsigned int bit = 0; bit < 64; bit++) {
        if (check(UINT64_MAX << bit, (int64_t)bit, "cleared low-bit prefix")) {
            return 1;
        }
        cases++;
    }
    for (unsigned int bit = 0; bit < 64; bit++) {
        uint64_t mask = UINT64_C(1) << bit;
        for (unsigned int high = bit + 1; high < 64; high++) {
            if (((high * 13U + bit * 7U) % 5U) != 0U) {
                mask |= UINT64_C(1) << high;
            }
        }
        if (check(mask, (int64_t)bit, "dense mixed mask")) {
            return 1;
        }
        cases++;
    }
    for (unsigned int i = 0; i < 10000; i++) {
        if (check_reference(next_random(&random_state), "deterministic random mask")) {
            return 1;
        }
        cases++;
    }
    printf("trailingZeros64: passed %" PRIu64 " cases\n", cases);
    return 0;
}
