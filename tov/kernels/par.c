/* par_for (see par.h) on a pthread pool, on every OS: on macOS it matches GCD.
 * Workers sleep until a job comes, then take indices from one shared counter,
 * so a slow chunk never holds up the rest. The calling thread works too. */
#define _GNU_SOURCE /* sched_getaffinity */
#include "par.h"
#include <pthread.h>
#include <stdatomic.h>
#include <unistd.h>
#ifdef __linux__
#include <sched.h>
#endif

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t go = PTHREAD_COND_INITIALIZER, done = PTHREAD_COND_INITIALIZER;
static pthread_once_t once = PTHREAD_ONCE_INIT;
static int workers;          /* threads besides the caller */
static unsigned long epoch;  /* bumped once per job */
static int busy;             /* workers not yet done with this job */
static struct {
    void *ctx;
    par_fn fn;
    size_t n;
    atomic_size_t next;
} job;

static void drain(void) {
    for (size_t i; (i = atomic_fetch_add(&job.next, 1)) < job.n;) job.fn(job.ctx, i);
}

static void *worker(void *arg) {
    unsigned long seen = 0;
    (void)arg;
    for (;;) {
        pthread_mutex_lock(&mu);
        while (epoch == seen) pthread_cond_wait(&go, &mu);
        seen = epoch;
        pthread_mutex_unlock(&mu);
        drain();
        pthread_mutex_lock(&mu);
        if (--busy == 0) pthread_cond_signal(&done);
        pthread_mutex_unlock(&mu);
    }
    return NULL;
}

static void start(void) {
    long cores = sysconf(_SC_NPROCESSORS_ONLN);
#ifdef __linux__
    cpu_set_t set; /* in a container or under taskset, only the cores we may use */
    if (sched_getaffinity(0, sizeof set, &set) == 0) cores = CPU_COUNT(&set);
#endif
    for (long t = 1; t < cores; t++) {
        pthread_t th;
        if (pthread_create(&th, NULL, worker, NULL) != 0) break;
        pthread_detach(th);
        workers++;
    }
}

void par_for(size_t n, void *ctx, par_fn fn) {
    pthread_once(&once, start);
    if (workers == 0 || n < 2) {
        for (size_t i = 0; i < n; i++) fn(ctx, i);
        return;
    }
    pthread_mutex_lock(&mu);
    job.ctx = ctx;
    job.fn = fn;
    job.n = n;
    atomic_store(&job.next, 0);
    busy = workers;
    epoch++;
    pthread_cond_broadcast(&go);
    pthread_mutex_unlock(&mu);
    drain();
    pthread_mutex_lock(&mu);
    while (busy) pthread_cond_wait(&done, &mu);
    pthread_mutex_unlock(&mu);
}
