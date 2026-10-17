/* par_for(n, ctx, fn): runs fn(ctx, i) for every i in [0, n) across every core
 * and returns when all are done, on a pthread pool (par.c). Call it from one
 * thread at a time, and not from inside fn. */
#pragma once
#include <stddef.h>

typedef void (*par_fn)(void *ctx, size_t i);
void par_for(size_t n, void *ctx, par_fn fn);
