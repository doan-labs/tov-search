{
    /* The first match at or after p1: start << 32 | end, or -1. */
    if (!gr_used || p1 > p0.len) return -1; /* gr_* in grepRe.c */
    const char *b = tv_arr_data(p0);
    regmatch_t m[1];
    m[0].rm_so = (regoff_t)p1;
    m[0].rm_eo = (regoff_t)p0.len;
    int flags = REG_STARTEND;
    if (p1 > 0 && b[p1 - 1] != '\n') flags |= REG_NOTBOL;
    if (regexec(&gr_re, b ? b : "", 1, m, flags) != 0) return -1;
    return ((int64_t)m[0].rm_so << 32) | (int64_t)m[0].rm_eo;
}
