/* Uses the libc, and sees when it's ready for that. */

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static char first_argument[64];
static int constructor_runs;

__attribute__((constructor)) static void construct(int argc, char** argv)
{
    constructor_runs++;
    if (argc > 0)
        snprintf(first_argument, sizeof(first_argument), "%s", argv[0]);
}

int constructed(void)
{
    return constructor_runs;
}

const char* argument_seen(void)
{
    return first_argument;
}

void replace_environment(void)
{
    setenv("FIL_RUST_RT_LIBC_ADDED", "1", 1);
}

int current_process(void)
{
    return getpid();
}

static void* double_it(void* argument)
{
    long* value = malloc(sizeof(long));
    *value = (long)argument * 2;
    return value;
}

long in_a_thread(void (*collect)(void), long value)
{
    pthread_t thread;
    if (pthread_create(&thread, NULL, double_it, (void*)value))
        return -1;
    collect();
    void* result;
    if (pthread_join(thread, &result))
        return -1;
    collect();
    return *(long*)result;
}

void churn_threads(int count)
{
    for (int index = 0; index < count; ++index) {
        pthread_t thread;
        pthread_create(&thread, NULL, double_it, NULL);
        pthread_join(thread, NULL);
    }
}
