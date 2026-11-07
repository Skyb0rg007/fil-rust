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

/* ---- Handlers and what they interrupt ---- */

#include <pthread.h>
#include <setjmp.h>
#include <stdlib.h>

/* The libc uses these for its critical sections. */
void zincrement_signal_deferral_depth(void);
void zdecrement_signal_deferral_depth(void);

static volatile int deferred_count;
static void count_it(int sig) { (void)sig; deferred_count++; }

static int install(int sig, void (*handler)(int), int flags)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = handler;
    sa.sa_flags = flags;
    sigemptyset(&sa.sa_mask);
    return sigaction(sig, &sa, NULL);
}

static void* poke(void* target)
{
    pthread_kill(*(pthread_t*)target, SIGUSR2);
    return NULL;
}

/* The signal lands on a thread running Fil-C code, so the handler can only run
   at the loop's next safepoint. */
int signal_while_running(void)
{
    if (install(SIGUSR2, count_it, 0))
        return -1;
    deferred_count = 0;
    pthread_t self = pthread_self(), other;
    pthread_create(&other, NULL, poke, &self);
    while (!deferred_count) { }
    pthread_join(other, NULL);
    return deferred_count;
}

/* More than anything keeps inline. Real-time signals all arrive. */
int signals_held_back(int count)
{
    int sig = SIGRTMIN + 4;
    if (install(sig, count_it, 0))
        return -1;
    deferred_count = 0;
    zincrement_signal_deferral_depth();
    for (int i = 0; i < count; i++)
        raise(sig);
    int before = deferred_count;
    zdecrement_signal_deferral_depth();
    return before ? -before : deferred_count;
}

static void* volatile kept;
static void (*volatile handler_collect)(void);
static void collect_in_handler(int sig)
{
    (void)sig;
    int* fresh = malloc(sizeof(int));
    *fresh = 41;
    handler_collect();
    kept = fresh;
}

int collection_in_handler(void (*collect)(void))
{
    handler_collect = collect;
    if (install(SIGUSR2, collect_in_handler, 0))
        return -1;
    raise(SIGUSR2);
    collect();
    return *(int*)kept + 1;
}

static sigjmp_buf out;
static void jump_out(int sig) { siglongjmp(out, sig); }

int jump_out_of_handler(void)
{
    if (install(SIGUSR2, jump_out, 0))
        return -1;
    int landed = sigsetjmp(out, 1);
    if (landed)
        return landed;
    raise(SIGUSR2);
    return -1;
}

static volatile int value_seen;
static void read_info(int sig, siginfo_t* info, void* context)
{
    (void)sig;
    (void)context;
    value_seen = info->si_value.sival_int;
}

int handler_sees_siginfo(int value)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = read_info;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGUSR2, &sa, NULL))
        return -1;
    union sigval sent;
    sent.sival_int = value;
    value_seen = 0;
    if (sigqueue(getpid(), SIGUSR2, sent))
        return -2;
    /* Sent to the process, so any thread may take it. */
    for (int i = 0; i < 5000 && !value_seen; i++)
        usleep(1000);
    return value_seen;
}

static volatile int asked_mask_blocked;
static void check_mask(int sig)
{
    (void)sig;
    sigset_t set;
    sigprocmask(SIG_BLOCK, NULL, &set);
    asked_mask_blocked = sigismember(&set, SIGTERM);
}

/* Deferred or not, a handler runs under the mask it asked for. */
int handler_mask_applies(void)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = check_mask;
    sigemptyset(&sa.sa_mask);
    sigaddset(&sa.sa_mask, SIGTERM);
    if (sigaction(SIGUSR2, &sa, NULL))
        return -1;
    asked_mask_blocked = 0;
    raise(SIGUSR2);
    int direct = asked_mask_blocked;
    asked_mask_blocked = 0;
    zincrement_signal_deferral_depth();
    raise(SIGUSR2);
    zdecrement_signal_deferral_depth();
    return direct * 2 + asked_mask_blocked;
}

/* sigaction tells the truth about what it replaced, including the flags. */
int sigaction_reports_the_old_handler(void)
{
    if (install(SIGUSR2, count_it, SA_RESTART))
        return -1;
    struct sigaction old;
    if (sigaction(SIGUSR2, NULL, &old))
        return -2;
    if (old.sa_handler != count_it || !(old.sa_flags & SA_RESTART))
        return -3;
    if (install(SIGUSR2, SIG_DFL, 0) || sigaction(SIGUSR2, NULL, &old))
        return -4;
    if (old.sa_handler != SIG_DFL)
        return -5;
    return 1;
}

int sigwait_takes_a_blocked_signal(void)
{
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    sigprocmask(SIG_BLOCK, &set, NULL);
    raise(SIGUSR2);
    int taken = 0;
    int result = sigwait(&set, &taken);
    sigprocmask(SIG_UNBLOCK, &set, NULL);
    return result ? -1 : taken;
}

int sigsuspend_waits_for_a_handler(void)
{
    if (install(SIGUSR2, count_it, 0))
        return -1;
    deferred_count = 0;
    sigset_t blocked, all_but;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGUSR2);
    sigprocmask(SIG_BLOCK, &blocked, NULL);
    pthread_t self = pthread_self(), other;
    pthread_create(&other, NULL, poke, &self);
    sigemptyset(&all_but);
    sigsuspend(&all_but);
    pthread_join(other, NULL);
    sigprocmask(SIG_UNBLOCK, &blocked, NULL);
    return deferred_count;
}

/* A program can always ask for the default action of a fault signal. */
int faults_can_be_left_at_default(void)
{
    return signal(SIGSEGV, SIG_DFL) == SIG_ERR ? -1 : 1;
}

/* The disposition changes with a signal queued for the old one. */
int disposition_changes_under_a_queued_signal(void)
{
    if (install(SIGUSR2, count_it, 0))
        return -1;
    deferred_count = 0;
    zincrement_signal_deferral_depth();
    raise(SIGUSR2);
    signal(SIGUSR2, SIG_IGN);
    zdecrement_signal_deferral_depth();
    for (volatile int i = 0; i < 1000; i++) { }
    signal(SIGUSR2, SIG_DFL);
    return 1;
}

int zsys_raise_without_exit(int);

/* A signal that's already here goes to the handler it was sent under. */
int queued_signal_reaches_the_old_handler(void)
{
    if (install(SIGUSR2, count_it, 0))
        return -1;
    deferred_count = 0;
    zsys_raise_without_exit(SIGUSR2);
    signal(SIGUSR2, SIG_DFL);
    return deferred_count;
}
