/* Signals with no libc main thread, timers, exit handlers and constructor
 * arguments, on the glibc flavor's libc. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t usr1_count;
static void on_usr1(int signal) { (void)signal; usr1_count++; }

int install_usr1(void) { return signal(SIGUSR1, on_usr1) == SIG_ERR ? -1 : 0; }
int usr1_seen(void) { return usr1_count; }

/* signal hands back what was there before. */
int signal_returns_the_old_handler(void)
{
    void (*first)(int) = signal(SIGWINCH, on_usr1);
    void (*second)(int) = signal(SIGWINCH, SIG_DFL);
    return first == SIG_DFL && second == on_usr1;
}

/* A timer whose signal comes to this process. */
int timer_fires(void)
{
    static volatile sig_atomic_t fired;
    struct sigaction action = { 0 };
    action.sa_handler = on_usr1;
    sigaction(SIGUSR2, &action, NULL);
    struct sigevent event = { 0 };
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR2;
    timer_t timer;
    if (timer_create(CLOCK_MONOTONIC, &event, &timer)) return -1;
    int before = usr1_count;
    struct itimerspec value = { .it_value = { .tv_nsec = 1000000 } };
    if (timer_settime(timer, 0, &value, NULL)) return -2;
    for (int i = 0; i < 2000 && usr1_count == before; i++) usleep(1000);
    struct itimerspec left;
    if (timer_gettime(timer, &left)) return -3;
    if (timer_delete(timer)) return -4;
    signal(SIGUSR2, SIG_DFL);
    (void)fired;
    return usr1_count > before;
}

static int compare(const void *a, const void *b) { (void)a; (void)b; return 0; }

/* dlopen and dlsym find Fil-C symbols. */
int dlsym_finds_qsort(void)
{
    void *self = dlopen(NULL, RTLD_NOW);
    if (!self) return -1;
    void (*sort)(void *, size_t, size_t, int (*)(const void *, const void *)) = dlsym(self, "qsort");
    if (!sort) return -2;
    int values[2] = { 2, 1 };
    sort(values, 2, sizeof *values, compare);
    dlclose(self);
    return 1;
}

/* Exit handlers, newest first, writing what they are. */
static void say(const char *what) { write(1, what, strlen(what)); }
static void plain(void) { say("atexit\n"); }
static void with_status(int status, void *argument) { say(argument); say(status == 7 ? "status 7\n" : "status ?\n"); }
static void quick(void) { say("quick\n"); }

/* glibc's at_quick_exit traps in Fil-C (docs/upstream-filc-issues.md, 8). */
extern int __cxa_at_quick_exit(void (*)(void), void *);
#define at_quick_exit(function) __cxa_at_quick_exit(function, NULL)

void exit_with_handlers(void)
{
    atexit(plain);
    on_exit(with_status, "on_exit ");
    at_quick_exit(quick);
    exit(7);
}

void quick_exit_with_handlers(void)
{
    atexit(plain);
    at_quick_exit(quick);
    quick_exit(3);
}

/* What constructors were given. */
static int seen_argc = -1;
static char seen_argv0[64];
static int seen_environ;

__attribute__((constructor)) static void look(int argc, char **argv, char **envp)
{
    seen_argc = argc;
    if (argc > 0 && argv[0]) strncpy(seen_argv0, argv[0], sizeof seen_argv0 - 1);
    for (char **e = envp; e && *e; e++)
        if (!strncmp(*e, "PATH=", 5)) seen_environ = 1;
}

int constructor_argc(void) { return seen_argc; }
int constructor_saw_path(void) { return seen_environ; }
int constructor_argv0_is(void) { return seen_argv0[0] != 0; }
