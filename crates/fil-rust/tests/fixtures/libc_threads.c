/* pthreads on the glibc flavor's libc. Each function returns 0 when what it
 * checks holds, and otherwise the number of the check that failed. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <semaphore.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(n, condition) do { if (!(condition)) return n; } while (0)

static void *give_back(void *argument) { return argument; }

int threads_and_results(void)
{
    pthread_t thread;
    int value = 7;
    void *result = NULL;
    CHECK(1, pthread_create(&thread, NULL, give_back, &value) == 0);
    CHECK(2, pthread_join(thread, &result) == 0);
    CHECK(3, result == &value && *(int *)result == 7);
    CHECK(4, pthread_equal(pthread_self(), pthread_self()));
    CHECK(5, !pthread_equal(pthread_self(), thread));
    CHECK(6, pthread_join(pthread_self(), NULL) == EDEADLK);
    pthread_attr_t attr;
    size_t size = 0;
    CHECK(7, pthread_attr_init(&attr) == 0);
    CHECK(8, pthread_attr_setstacksize(&attr, 1 << 20) == 0);
    CHECK(9, pthread_attr_getstacksize(&attr, &size) == 0 && size == 1 << 20);
    CHECK(10, pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED) == 0);
    int state = -1;
    CHECK(11, pthread_attr_getdetachstate(&attr, &state) == 0 && state == PTHREAD_CREATE_DETACHED);
    CHECK(12, pthread_attr_setdetachstate(&attr, 99) == EINVAL);
    CHECK(13, pthread_create(&thread, &attr, give_back, NULL) == 0);
    pthread_attr_destroy(&attr);
    CHECK(14, pthread_setname_np(pthread_self(), "named") == 0);
    char name[16];
    CHECK(15, pthread_getname_np(pthread_self(), name, sizeof name) == 0 && !strcmp(name, "named"));
    CHECK(16, pthread_setname_np(pthread_self(), "a name much too long") == ERANGE);
    return 0;
}

static void *exit_early(void *argument)
{
    pthread_exit((char *)argument + 1);
    return NULL;
}

int exit_hands_back_a_result(void)
{
    pthread_t thread;
    char bytes[2];
    void *result = NULL;
    CHECK(1, pthread_create(&thread, NULL, exit_early, bytes) == 0);
    CHECK(2, pthread_join(thread, &result) == 0 && result == bytes + 1);
    return 0;
}

int mutex_kinds(void)
{
    pthread_mutex_t normal = PTHREAD_MUTEX_INITIALIZER;
    CHECK(1, pthread_mutex_lock(&normal) == 0);
    CHECK(2, pthread_mutex_trylock(&normal) == EBUSY);
    CHECK(3, pthread_mutex_unlock(&normal) == 0);
    CHECK(4, pthread_mutex_destroy(&normal) == 0);

    pthread_mutex_t recursive = PTHREAD_RECURSIVE_MUTEX_INITIALIZER_NP;
    CHECK(5, pthread_mutex_lock(&recursive) == 0);
    CHECK(6, pthread_mutex_lock(&recursive) == 0);
    CHECK(7, pthread_mutex_trylock(&recursive) == 0);
    CHECK(8, pthread_mutex_unlock(&recursive) == 0 && pthread_mutex_unlock(&recursive) == 0);
    CHECK(9, pthread_mutex_unlock(&recursive) == 0);
    CHECK(10, pthread_mutex_unlock(&recursive) == EPERM);

    pthread_mutexattr_t attr;
    pthread_mutex_t checked;
    int kind = -1;
    CHECK(11, pthread_mutexattr_init(&attr) == 0);
    CHECK(12, pthread_mutexattr_settype(&attr, PTHREAD_MUTEX_ERRORCHECK) == 0);
    CHECK(13, pthread_mutexattr_gettype(&attr, &kind) == 0 && kind == PTHREAD_MUTEX_ERRORCHECK);
    CHECK(14, pthread_mutexattr_settype(&attr, 42) == EINVAL);
    CHECK(15, pthread_mutex_init(&checked, &attr) == 0);
    pthread_mutexattr_destroy(&attr);
    CHECK(16, pthread_mutex_unlock(&checked) == EPERM);
    CHECK(17, pthread_mutex_lock(&checked) == 0);
    CHECK(18, pthread_mutex_lock(&checked) == EDEADLK);
    CHECK(19, pthread_mutex_unlock(&checked) == 0);

    struct timespec soon;
    clock_gettime(CLOCK_REALTIME, &soon);
    soon.tv_nsec += 20 * 1000 * 1000;
    if (soon.tv_nsec >= 1000000000) { soon.tv_sec++; soon.tv_nsec -= 1000000000; }
    CHECK(20, pthread_mutex_lock(&normal) == 0);
    CHECK(21, pthread_mutex_timedlock(&normal, &soon) == ETIMEDOUT);
    soon.tv_nsec = 2000000000;
    CHECK(22, pthread_mutex_timedlock(&normal, &soon) == EINVAL);
    pthread_mutex_unlock(&normal);
    return 0;
}

/* Many threads adding under one lock: contended_mutex's own count, and one
 * Rust threads share. */
struct count { pthread_mutex_t lock; long value; };
static struct count own = { PTHREAD_MUTEX_INITIALIZER, 0 }, shared = { PTHREAD_MUTEX_INITIALIZER, 0 };

static void add(struct count *count, int times)
{
    for (int i = 0; i < times; i++) {
        pthread_mutex_lock(&count->lock);
        long seen = count->value;
        sched_yield();
        count->value = seen + 1;
        pthread_mutex_unlock(&count->lock);
    }
}

void count_up(int times) { add(&shared, times); }
long counted_so_far(void) { return shared.value; }

static void *add_own(void *argument) { add(&own, 200); return argument; }
static void *add_shared(void *argument) { add(&shared, 200); return argument; }

int contended_mutex(void)
{
    pthread_t threads[6];
    own.value = 0;
    for (int i = 0; i < 6; i++)
        CHECK(1, pthread_create(&threads[i], NULL, add_own, NULL) == 0);
    for (int i = 0; i < 6; i++)
        pthread_join(threads[i], NULL);
    CHECK(2, own.value == 1200);
    return 0;
}

/* Fil-C threads counting alongside whoever else is. */
void count_in_threads(void)
{
    pthread_t threads[3];
    for (int i = 0; i < 3; i++)
        pthread_create(&threads[i], NULL, add_shared, NULL);
    for (int i = 0; i < 3; i++)
        pthread_join(threads[i], NULL);
}

/* A bounded queue between producers and consumers. */
static pthread_mutex_t queue_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t not_empty = PTHREAD_COND_INITIALIZER, not_full = PTHREAD_COND_INITIALIZER;
static int queue[4], queued, head;
static long consumed_total;

static void *produce(void *argument)
{
    for (int i = 1; i <= 500; i++) {
        pthread_mutex_lock(&queue_lock);
        while (queued == 4)
            pthread_cond_wait(&not_full, &queue_lock);
        queue[(head + queued++) % 4] = i;
        pthread_cond_signal(&not_empty);
        pthread_mutex_unlock(&queue_lock);
    }
    return argument;
}

static void *consume(void *argument)
{
    for (int i = 0; i < 500; i++) {
        pthread_mutex_lock(&queue_lock);
        while (queued == 0)
            pthread_cond_wait(&not_empty, &queue_lock);
        consumed_total += queue[head];
        head = (head + 1) % 4;
        queued--;
        pthread_cond_broadcast(&not_full);
        pthread_mutex_unlock(&queue_lock);
    }
    return argument;
}

int condition_variables(void)
{
    pthread_t threads[4];
    consumed_total = 0;
    pthread_create(&threads[0], NULL, produce, NULL);
    pthread_create(&threads[1], NULL, produce, NULL);
    pthread_create(&threads[2], NULL, consume, NULL);
    pthread_create(&threads[3], NULL, consume, NULL);
    for (int i = 0; i < 4; i++)
        pthread_join(threads[i], NULL);
    CHECK(1, consumed_total == 2 * 500 * 501 / 2);

    pthread_cond_t cond;
    pthread_condattr_t attr;
    clockid_t clock = -1;
    CHECK(2, pthread_condattr_init(&attr) == 0);
    CHECK(3, pthread_condattr_setclock(&attr, CLOCK_MONOTONIC) == 0);
    CHECK(4, pthread_condattr_getclock(&attr, &clock) == 0 && clock == CLOCK_MONOTONIC);
    CHECK(5, pthread_cond_init(&cond, &attr) == 0);
    struct timespec start, deadline, end;
    clock_gettime(CLOCK_MONOTONIC, &start);
    deadline = start;
    deadline.tv_nsec += 30 * 1000 * 1000;
    if (deadline.tv_nsec >= 1000000000) { deadline.tv_sec++; deadline.tv_nsec -= 1000000000; }
    pthread_mutex_lock(&queue_lock);
    int waited = pthread_cond_timedwait(&cond, &queue_lock, &deadline);
    pthread_mutex_unlock(&queue_lock);
    clock_gettime(CLOCK_MONOTONIC, &end);
    CHECK(6, waited == ETIMEDOUT);
    long elapsed = (end.tv_sec - start.tv_sec) * 1000000000L + end.tv_nsec - start.tv_nsec;
    CHECK(7, elapsed >= 25 * 1000 * 1000);
    CHECK(8, pthread_cond_destroy(&cond) == 0);
    return 0;
}

static pthread_rwlock_t rwlock = PTHREAD_RWLOCK_INITIALIZER;
static long shared_value;

static void *writer(void *argument)
{
    for (int i = 0; i < 300; i++) {
        pthread_rwlock_wrlock(&rwlock);
        shared_value++;
        pthread_rwlock_unlock(&rwlock);
    }
    return argument;
}

static void *reader(void *argument)
{
    long last = 0;
    for (int i = 0; i < 300; i++) {
        pthread_rwlock_rdlock(&rwlock);
        if (shared_value < last)
            return (void *)1;
        last = shared_value;
        pthread_rwlock_unlock(&rwlock);
    }
    return argument;
}

int reader_writer_locks(void)
{
    CHECK(1, pthread_rwlock_rdlock(&rwlock) == 0 && pthread_rwlock_tryrdlock(&rwlock) == 0);
    CHECK(2, pthread_rwlock_trywrlock(&rwlock) == EBUSY);
    CHECK(3, pthread_rwlock_unlock(&rwlock) == 0 && pthread_rwlock_unlock(&rwlock) == 0);
    CHECK(4, pthread_rwlock_wrlock(&rwlock) == 0 && pthread_rwlock_tryrdlock(&rwlock) == EBUSY);
    CHECK(5, pthread_rwlock_unlock(&rwlock) == 0);
    pthread_t threads[4];
    shared_value = 0;
    for (int i = 0; i < 4; i++)
        pthread_create(&threads[i], NULL, i % 2 ? reader : writer, NULL);
    for (int i = 0; i < 4; i++) {
        void *result;
        pthread_join(threads[i], &result);
        CHECK(6, result == NULL);
    }
    CHECK(7, shared_value == 600);
    return 0;
}

static pthread_once_t once = PTHREAD_ONCE_INIT;
static int once_runs;
static void run_once(void) { usleep(10000); once_runs++; }
static void *once_caller(void *argument) { pthread_once(&once, run_once); return argument; }

int once_runs_once(void)
{
    pthread_t threads[5];
    for (int i = 0; i < 5; i++)
        pthread_create(&threads[i], NULL, once_caller, NULL);
    for (int i = 0; i < 5; i++)
        pthread_join(threads[i], NULL);
    pthread_once(&once, run_once);
    CHECK(1, once_runs == 1);
    return 0;
}

/* Key destructors run as threads Fil-C created end, however they end. */
static pthread_key_t key;
static int destructed, reset_once;

static void destructor(void *value)
{
    destructed += *(int *)value;
    free(value);
    /* Setting a value again gets the destructor called again. */
    if (!reset_once) {
        reset_once = 1;
        int *again = malloc(sizeof *again);
        *again = 1000;
        pthread_setspecific(key, again);
    }
}

static void *keep_value(void *argument)
{
    int *value = malloc(sizeof *value);
    *value = (int)(intptr_t)argument;
    pthread_setspecific(key, value);
    if (pthread_getspecific(key) != value)
        return (void *)1;
    if ((intptr_t)argument == 10)
        pthread_exit(NULL);
    return NULL;
}

int key_destructors(void)
{
    CHECK(1, pthread_key_create(&key, destructor) == 0);
    CHECK(2, pthread_getspecific(key) == NULL);
    int mine = 5;
    CHECK(3, pthread_setspecific(key, &mine) == 0 && pthread_getspecific(key) == &mine);
    pthread_t threads[2];
    pthread_create(&threads[0], NULL, keep_value, (void *)(intptr_t)1);
    pthread_create(&threads[1], NULL, keep_value, (void *)(intptr_t)10);
    for (int i = 0; i < 2; i++) {
        void *result;
        pthread_join(threads[i], &result);
        CHECK(4, result == NULL);
    }
    CHECK(5, destructed == 1011);
    /* This thread's value is its own. */
    CHECK(6, pthread_getspecific(key) == &mine);
    CHECK(7, pthread_key_delete(key) == 0);
    CHECK(8, pthread_setspecific(key, &mine) == EINVAL);
    pthread_key_t other;
    CHECK(9, pthread_key_create(&other, NULL) == 0 && pthread_getspecific(other) == NULL);
    pthread_key_delete(other);
    return 0;
}

static pthread_barrier_t barrier;
static int before_barrier, after_mismatch;

static void *at_barrier(void *argument)
{
    __atomic_fetch_add(&before_barrier, 1, __ATOMIC_SEQ_CST);
    int result = pthread_barrier_wait(&barrier);
    if (__atomic_load_n(&before_barrier, __ATOMIC_SEQ_CST) != 4)
        __atomic_fetch_add(&after_mismatch, 1, __ATOMIC_SEQ_CST);
    return (void *)(intptr_t)(result == PTHREAD_BARRIER_SERIAL_THREAD);
}

int barriers_semaphores_spinlocks(void)
{
    CHECK(1, pthread_barrier_init(&barrier, NULL, 0) == EINVAL);
    CHECK(2, pthread_barrier_init(&barrier, NULL, 4) == 0);
    pthread_t threads[4];
    for (int i = 0; i < 4; i++)
        pthread_create(&threads[i], NULL, at_barrier, NULL);
    int serial = 0;
    for (int i = 0; i < 4; i++) {
        void *result;
        pthread_join(threads[i], &result);
        serial += (int)(intptr_t)result;
    }
    CHECK(3, serial == 1 && after_mismatch == 0);
    pthread_barrier_destroy(&barrier);

    sem_t sem;
    int value = -1;
    CHECK(4, sem_init(&sem, 0, 2) == 0);
    CHECK(5, sem_wait(&sem) == 0 && sem_trywait(&sem) == 0);
    CHECK(6, sem_trywait(&sem) == -1 && errno == EAGAIN);
    CHECK(7, sem_post(&sem) == 0 && sem_getvalue(&sem, &value) == 0 && value == 1);
    struct timespec past = { 0, 0 };
    CHECK(8, sem_wait(&sem) == 0 && sem_timedwait(&sem, &past) == -1 && errno == ETIMEDOUT);
    sem_destroy(&sem);

    pthread_spinlock_t spin;
    CHECK(9, pthread_spin_init(&spin, PTHREAD_PROCESS_PRIVATE) == 0);
    CHECK(10, pthread_spin_lock(&spin) == 0 && pthread_spin_trylock(&spin) == EBUSY);
    CHECK(11, pthread_spin_unlock(&spin) == 0 && pthread_spin_trylock(&spin) == 0);
    pthread_spin_unlock(&spin);
    pthread_spin_destroy(&spin);
    return 0;
}

static int prepared, in_parent, in_child;
static void prepare(void) { prepared++; }
static void parent(void) { in_parent++; }
static void child(void) { in_child++; }

int fork_handlers(void)
{
    CHECK(1, pthread_atfork(prepare, parent, child) == 0);
    pid_t pid = fork();
    if (pid == 0)
        _exit(in_child == 1 && prepared == 1 && in_parent == 0 ? 0 : 1);
    int status;
    CHECK(2, pid > 0 && waitpid(pid, &status, 0) == pid);
    CHECK(3, WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(4, prepared == 1 && in_parent == 1 && in_child == 0);
    return 0;
}

/* pthread_exit on a thread Fil-C didn't create. */
void exit_from_rust(void) { pthread_exit(NULL); }

/* A mutex that's been freed. */
int lock_freed_mutex(void)
{
    pthread_mutex_t *mutex = malloc(sizeof *mutex);
    pthread_mutex_init(mutex, NULL);
    free(mutex);
    return pthread_mutex_lock(mutex);
}
