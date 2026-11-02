/* ---- Signals on threads the Fil-C runtime doesn't know ----

   Shared by the musl and glibc stand-ins.

   The runtime's native signal handler assumes it only runs on Fil-C threads:
   in a pure Fil-C process its own service threads block user signals. Here
   the process also has Rust threads, which don't, so a process-directed
   signal for which Fil-C code installed a handler can land on one, and the
   runtime would crash looking up its thread.

   fil-rust points libpizlo's `sigaction` import at fil_rust_sigaction, which
   installs fr_signal_trampoline in place of the runtime's handler and keeps
   the latter in fr_handlers. On a Fil-C thread the trampoline calls the
   runtime's handler as before; on any other thread it re-queues the signal,
   with its siginfo, to the Fil-C main thread (which always exists and takes
   user signals). Queries report the runtime's handler back, as the runtime
   expects. Synchronous fault signals (SIGSEGV, SIGBUS, ...) are never
   affected: Fil-C doesn't let programs handle them, so Rust's stack
   overflow handler stays in place. */
#include <dlfcn.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define FR_NSIG 65

static void* (*volatile fr_get_my_thread)(void);
static volatile pid_t fr_signal_target;
static void* volatile fr_handlers[FR_NSIG];
static volatile int fr_handler_siginfo[FR_NSIG];
static pthread_mutex_t fr_signal_lock = PTHREAD_MUTEX_INITIALIZER;

static void fr_signal_trampoline(int sig, siginfo_t* info, void* context)
{
    if (sig <= 0 || sig >= FR_NSIG)
        return;
    void* (*get)(void) = fr_get_my_thread;
    void* handler = fr_handlers[sig];
    if (get && get()) {
        if (!handler)
            return;
        if (fr_handler_siginfo[sig])
            ((void (*)(int, siginfo_t*, void*))handler)(sig, info, context);
        else
            ((void (*)(int))handler)(sig);
        return;
    }
    pid_t target = fr_signal_target;
    if (target) {
        int saved = errno;
        /* The kernel only lets a thread queue a kernel- or kill()-style
           siginfo (si_code >= 0) to itself, so those are re-sent with tgkill
           (the handler then sees SI_TKILL from this process); sigqueue-style
           ones keep their siginfo. */
        if (!info || info->si_code >= 0 || syscall(SYS_rt_tgsigqueueinfo, getpid(), target, sig, info) != 0)
            syscall(SYS_tgkill, getpid(), target, sig);
        errno = saved;
    }
}

static int fr_is_real_handler(const struct sigaction* act)
{
    void* h = (act->sa_flags & SA_SIGINFO) ? (void*)act->sa_sigaction : (void*)act->sa_handler;
    return h != (void*)SIG_DFL && h != (void*)SIG_IGN && h != (void*)fr_signal_trampoline;
}

int fil_rust_sigaction(int sig, const struct sigaction* act, struct sigaction* oact)
{
    if (sig <= 0 || sig >= FR_NSIG)
        return sigaction(sig, act, oact);
    pthread_mutex_lock(&fr_signal_lock);
    void* old_handler = fr_handlers[sig];
    int old_siginfo = fr_handler_siginfo[sig];
    struct sigaction installed;
    const struct sigaction* to_install = act;
    if (act && fr_is_real_handler(act)) {
        installed = *act;
        fr_handler_siginfo[sig] = (act->sa_flags & SA_SIGINFO) != 0;
        fr_handlers[sig] = fr_handler_siginfo[sig] ? (void*)act->sa_sigaction : (void*)act->sa_handler;
        installed.sa_sigaction = fr_signal_trampoline;
        installed.sa_flags |= SA_SIGINFO;
        to_install = &installed;
    }
    int result = sigaction(sig, to_install, oact);
    if (result == 0 && oact && (void*)oact->sa_sigaction == (void*)fr_signal_trampoline) {
        if (old_siginfo) {
            oact->sa_sigaction = (void (*)(int, siginfo_t*, void*))old_handler;
        } else {
            oact->sa_handler = (void (*)(int))old_handler;
            oact->sa_flags &= ~SA_SIGINFO;
        }
    }
    if (result != 0 && act) {
        fr_handlers[sig] = old_handler;
        fr_handler_siginfo[sig] = old_siginfo;
    }
    pthread_mutex_unlock(&fr_signal_lock);
    return result;
}

/* Called by fil-rust once the runtime is up: `get_my_thread` is the
   runtime's filc_get_my_thread, `target` the Fil-C main thread's tid, and
   `runtime_base` libpizlo's load address. Handlers the runtime installed
   before its sigaction import was redirected are adopted. */
void fil_rust_signals_init(void* (*get_my_thread)(void), pid_t target, void* runtime_base)
{
    fr_get_my_thread = get_my_thread;
    fr_signal_target = target;
    for (int sig = 1; sig < FR_NSIG; sig++) {
        struct sigaction current;
        if (sigaction(sig, NULL, &current) || !fr_is_real_handler(&current))
            continue;
        void* h = (current.sa_flags & SA_SIGINFO) ? (void*)current.sa_sigaction : (void*)current.sa_handler;
        Dl_info info;
        if (dladdr(h, &info) && info.dli_fbase == runtime_base)
            fil_rust_sigaction(sig, &current, NULL);
    }
}
