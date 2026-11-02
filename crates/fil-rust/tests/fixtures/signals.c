// Signals and stack overflow in Fil-C code running inside a Rust process.
#include <signal.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t usr1_count, alarm_count;

static void on_usr1(int sig) { (void)sig; usr1_count++; }
static void on_alarm(int sig) { (void)sig; alarm_count++; }

/* Installs a SIGUSR1 handler with sigaction and raises it n times. */
int raise_usr1(int n)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGUSR1, &sa, NULL))
        return -1;
    usr1_count = 0;
    for (int i = 0; i < n; i++)
        raise(SIGUSR1);
    return usr1_count;
}

int usr1_count_now(void) { return usr1_count; }

/* Waits (in Fil-C) until `count` SIGUSR1s have arrived or `ms` pass. */
int wait_usr1(int count, int ms)
{
    for (int i = 0; i < ms && usr1_count < count; i++)
        usleep(1000);
    return usr1_count;
}

/* Arms a repeating 5ms interval timer and waits for `n` SIGALRMs. */
int interval_timer(int n)
{
    signal(SIGALRM, on_alarm);
    alarm_count = 0;
    struct itimerval it = { { 0, 5000 }, { 0, 5000 } };
    setitimer(ITIMER_REAL, &it, NULL);
    for (int i = 0; i < 2000 && alarm_count < n; i++)
        usleep(1000);
    struct itimerval off = { { 0, 0 }, { 0, 0 } };
    setitimer(ITIMER_REAL, &off, NULL);
    signal(SIGALRM, SIG_DFL);
    return alarm_count >= n ? n : alarm_count;
}

/* Recurses until the stack runs out. */
int recurse_forever(int depth)
{
    volatile char pad[256];
    pad[0] = (char)depth;
    return recurse_forever(depth + 1) + pad[0];
}
