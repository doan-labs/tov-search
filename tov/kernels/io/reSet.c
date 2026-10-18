#include <regex.h>
#include <stdlib.h>
#include <string.h>
typedef void (*par_fn)(void *ctx, size_t i);
void par_for(size_t n, void *ctx, par_fn fn);

/* Slot 0: re: (names), slot 1: path: (whole paths). */
static regex_t rx_slot[2];
static int rx_used[2];

/* Perl's \d \w \s (and capitals) as bracket expressions, outside brackets. */
static char *rx_translate(const char *p) {
    size_t n = strlen(p);
    char *o = malloc(n * 12 + 1), *w = o;
    int in = 0;
    for (size_t i = 0; i < n; i++) {
        const char *x = NULL;
        if (!in && p[i] == '\\' && i + 1 < n) {
            switch (p[i + 1]) {
            case 'd': x = "[0-9]"; break;
            case 'D': x = "[^0-9]"; break;
            case 'w': x = "[A-Za-z0-9_]"; break;
            case 'W': x = "[^A-Za-z0-9_]"; break;
            case 's': x = "[ \t\n\r\f\v]"; break;
            case 'S': x = "[^ \t\n\r\f\v]"; break;
            }
        }
        if (x) {
            strcpy(w, x);
            w += strlen(x);
            i++;
            continue;
        }
        if (!in && p[i] == '\\' && i + 1 < n) {
            *w++ = p[i++];
            *w++ = p[i];
            continue;
        }
        if (p[i] == '[' && !in) {
            in = 1;
            *w++ = p[i];
            if (i + 1 < n && p[i + 1] == '^') *w++ = p[++i];
            if (i + 1 < n && p[i + 1] == ']') *w++ = p[++i]; /* a leading ] is literal */
            continue;
        }
        if (p[i] == ']' && in) in = 0;
        *w++ = p[i];
    }
    *w = 0;
    return o;
}

static int rx_match(int64_t slot, const char *b, size_t n) {
    char buf[1024], *s = n < sizeof buf ? buf : malloc(n + 1);
    memcpy(s, b, n);
    s[n] = 0;
    int ok = regexec(&rx_slot[slot], s, 0, NULL, 0) == 0;
    if (s != buf) free(s);
    return ok;
}

typedef struct { int64_t slot; const uint8_t *names; const uint32_t *off; uint8_t *out; } rx_job;

static void rx_one(void *ctx, size_t k) {
    rx_job *j = ctx;
    j->out[k] = (uint8_t)rx_match(j->slot, (const char *)j->names + j->off[k], j->off[k + 1] - j->off[k]);
}

static tv_str rx_names(int64_t slot, const uint8_t *names, const uint32_t *off, int64_t u) {
    uint8_t *out = calloc((size_t)u + 1, 1);
    rx_job j = {slot, names, off, out};
    par_for((size_t)u, &j, rx_one);
    tv_str s = tv_str_from((const char *)out, (size_t)u);
    free(out);
    return s;
}
{
    if (rx_used[p0]) regfree(&rx_slot[p0]);
    char *pat = rx_translate(p1.p->data);
    int r = regcomp(&rx_slot[p0], pat, REG_EXTENDED | REG_ICASE | REG_NOSUB);
    free(pat);
    rx_used[p0] = r == 0;
    if (r == 0) return TV_EMPTY_STR;
    char msg[256];
    regerror(r, &rx_slot[p0], msg, sizeof msg);
    return tv_str_from(msg, strlen(msg));
}
