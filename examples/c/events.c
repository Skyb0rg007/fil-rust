#include <pthread.h>
#include <stdlib.h>
#include <string.h>

/* Sorts strings with a caller-supplied comparator. */
typedef int (*str_cmp)(const char*, const char*);

static str_cmp current_cmp;

static int qsort_adapter(const void* a, const void* b)
{
    return current_cmp(*(const char* const*)a, *(const char* const*)b);
}

void sort_strings(const char** items, size_t n, str_cmp cmp)
{
    current_cmp = cmp;
    qsort(items, n, sizeof *items, qsort_adapter);
}

/* A tiny event bus: handlers are registered, then events are dispatched from
   a pool of C threads. */
typedef void (*handler)(int worker, int event);

#define MAX_HANDLERS 8
static handler handlers[MAX_HANDLERS];
static int handler_count;
static pthread_mutex_t bus_lock = PTHREAD_MUTEX_INITIALIZER;

int subscribe(handler h)
{
    pthread_mutex_lock(&bus_lock);
    int id = handler_count < MAX_HANDLERS ? handler_count++ : -1;
    if (id >= 0)
        handlers[id] = h;
    pthread_mutex_unlock(&bus_lock);
    return id;
}

void unsubscribe_all(void)
{
    pthread_mutex_lock(&bus_lock);
    handler_count = 0;
    pthread_mutex_unlock(&bus_lock);
}

struct job {
    int worker, first, count;
};

static void* dispatch_range(void* arg)
{
    struct job* job = arg;
    for (int e = job->first; e < job->first + job->count; e++) {
        pthread_mutex_lock(&bus_lock);
        int n = handler_count;
        handler copy[MAX_HANDLERS];
        memcpy(copy, handlers, sizeof copy);
        pthread_mutex_unlock(&bus_lock);
        for (int i = 0; i < n; i++)
            copy[i](job->worker, e);
    }
    return NULL;
}

/* Dispatches events 0..total-1 across `workers` threads; returns when all
   handlers have run. */
void dispatch(int workers, int total)
{
    pthread_t* threads = calloc(workers, sizeof *threads);
    struct job* jobs = calloc(workers, sizeof *jobs);
    int per = total / workers;
    for (int w = 0; w < workers; w++) {
        jobs[w] = (struct job){ w, w * per, w == workers - 1 ? total - w * per : per };
        pthread_create(&threads[w], NULL, dispatch_range, &jobs[w]);
    }
    for (int w = 0; w < workers; w++)
        pthread_join(threads[w], NULL);
    free(threads);
    free(jobs);
}
