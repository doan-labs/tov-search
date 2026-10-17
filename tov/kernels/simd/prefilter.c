#include "simd.h"

int64_t k_prefilter(const k_prefilter_args *a) {
    /* cand holds u ids, so the branchless writes stay inside it. */
    return prefilter_range(a->umask, 0, a->u, a->nt, a->tMask, a->tLoose, a->tStart, a->cand);
}
