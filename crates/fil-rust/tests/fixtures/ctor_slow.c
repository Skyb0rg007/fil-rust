/* A constructor that takes a while, then creates a thread: the loader holds
   its lock all the while, and creating a Fil-C thread waits for every thread
   in the runtime. */
#include <pthread.h>
#include <unistd.h>

static int started;

static void *work(void *arg) {
    started = 1;
    return arg;
}

__attribute__((constructor)) static void start(void) {
    usleep(300 * 1000);
    pthread_t t;
    pthread_create(&t, 0, work, 0);
    pthread_join(t, 0);
}

int ctor_slow_started(void) { return started; }
