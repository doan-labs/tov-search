int64_t k_trailingZeros64(const k_trailingZeros64_args *a) {
    return a->bits ? __builtin_ctzll(a->bits) : 64;
}
