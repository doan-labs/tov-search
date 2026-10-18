#include <regex.h>
#include <stdlib.h>
#include <string.h>

/* The content search's regex: lines are separate (^ $ at each line, . and
 * [^...] never match a newline). */
static regex_t gr_re;
static int gr_used;
{
    if (gr_used) regfree(&gr_re);
    char *pat = rx_translate(p0.p->data); /* in reSet.c */
    int flags = REG_EXTENDED | REG_NEWLINE | (p1 ? REG_ICASE : 0);
#ifdef REG_ENHANCED
    flags |= REG_ENHANCED; /* \b and friends, as glibc has them anyway */
#endif
    int r = regcomp(&gr_re, pat, flags);
    free(pat);
    gr_used = r == 0;
    if (r == 0) return TV_EMPTY_STR;
    char msg[256];
    regerror(r, &gr_re, msg, sizeof msg);
    return tv_str_from(msg, strlen(msg));
}
