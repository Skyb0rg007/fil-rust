/* Starts threads from a constructor, while the library is being loaded. */
#include <pthread.h>
#include <stdatomic.h>

static atomic_int counter;
static pthread_t threads[4];

static void *work(void *arg) {
    for (int i = 0; i < 1000; i++) atomic_fetch_add(&counter, 1);
    return arg;
}

__attribute__((constructor)) static void start(void) {
    for (int i = 0; i < 4; i++) pthread_create(&threads[i], 0, work, 0);
}

int join_ctor_threads(void) {
    for (int i = 0; i < 4; i++) pthread_join(threads[i], 0);
    return atomic_load(&counter);
}
