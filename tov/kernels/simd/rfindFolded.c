#include "simd.h"

int64_t k_rfindFolded(const k_rfindFolded_args *a) {
    return rfind_folded(a->unames, a->from, a->to, (uint8_t)a->c);
}
