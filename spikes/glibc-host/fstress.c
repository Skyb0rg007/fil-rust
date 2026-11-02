#include <dirent.h>
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static long total;
static void *worker(void *arg) {
    long n = 0;
    for (int i = 0; i < 200000; i++) {
        char *s = malloc(32 + (i % 200));
        snprintf(s, 32, "%ld-%d", (long)arg, i);
        n += strlen(s);
        if (i % 3) free(s);
    }
    pthread_mutex_lock(&mu); total += n; pthread_cond_broadcast(&cv); pthread_mutex_unlock(&mu);
    return (void *)n;
}
static volatile sig_atomic_t got;
static void on_usr1(int s) { got = s; }
int main(void) {
    pthread_t t[4];
    for (long i = 0; i < 4; i++) pthread_create(&t[i], NULL, worker, (void *)i);
    pthread_mutex_lock(&mu);
    struct timespec ts; clock_gettime(CLOCK_REALTIME, &ts); ts.tv_nsec += 1000000; if (ts.tv_nsec >= 1000000000) { ts.tv_sec++; ts.tv_nsec -= 1000000000; }
    int r = pthread_cond_timedwait(&cv, &mu, &ts);
    pthread_mutex_unlock(&mu);
    for (int i = 0; i < 4; i++) pthread_join(t[i], NULL);
    printf("threads ok total=%ld timedwait=%s\n", total, r == ETIMEDOUT ? "ETIMEDOUT" : r == 0 ? "0" : "other");
    struct stat st; int sr = stat("/etc/hostname", &st);
    struct stat st2; stat(".", &st2);
    printf("stat ok=%d size=%lld mode=%o ino=%llu dirmode_isdir=%d mtime=%lld\n", sr == 0, (long long)st.st_size, st.st_mode & 0777, (unsigned long long)st.st_ino, S_ISDIR(st2.st_mode), (long long)st.st_mtim.tv_sec);
    DIR *d = opendir("."); int cnt = 0, sawc = 0; struct dirent *e;
    while ((e = readdir(d))) { cnt++; if (!strcmp(e->d_name, "host")) sawc = e->d_type == DT_REG; }
    closedir(d);
    printf("readdir entries=%d saw_host_as_regular=%d\n", cnt, sawc);
    signal(SIGUSR1, on_usr1); raise(SIGUSR1);
    printf("signal handled=%d\n", got == SIGUSR1);
    return 0;
}
