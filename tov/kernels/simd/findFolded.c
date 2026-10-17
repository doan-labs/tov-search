#include "simd.h"

int64_t k_findFolded(const k_findFolded_args *a) {
    return find_folded(a->unames, a->from, a->to, (uint8_t)a->c);
}
