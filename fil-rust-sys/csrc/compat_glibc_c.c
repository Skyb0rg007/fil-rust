/* Stand-in for the yolo glibc of Fil-C's /opt/fil distribution
   (libyolocimpl.so) on top of the host glibc.

   Fil-C objects bind most of their yolo-libc imports straight to the host
   glibc: glibc's dynamic linker matches versioned references by version
   name, in any object of the global scope. This library supplies the
   soname, the version definitions (compat_glibc_c.map), and the few
   non-standard symbols the runtime imports. Their behavior follows the
   distribution's own libyolocimpl.so: futex helpers return 0 or a positive
   errno, and glibc reserves SIGCANCEL (32) and SIGSETXID (33). */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stddef.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
#include <linux/futex.h>

int libc_internal_signals[2] = { 32, 33 };
unsigned num_libc_internal_signals = 2;

int sigdelsetyolo(sigset_t *set, int sig)
{
    unsigned s = sig - 1;
    if (s >= 64 || !set) {
        errno = EINVAL;
        return -1;
    }
    ((unsigned long *)set)[0] &= ~(1UL << s);
    return 0;
}

static int fop(int priv, int op)
{
    return priv ? (op | FUTEX_PRIVATE_FLAG) : op;
}

void yolo_futex_wake(volatile int *addr, int cnt, int priv)
{
    syscall(SYS_futex, addr, fop(priv, FUTEX_WAKE), cnt);
}

void yolo_futex_wait(volatile int *addr, int val, int priv)
{
    syscall(SYS_futex, addr, fop(priv, FUTEX_WAIT), val, NULL);
}

/* Absolute timeout on clock_id (CLOCK_REALTIME or CLOCK_MONOTONIC). */
int yolo_futex_timedwait(volatile int *addr, int val, int clock_id,
                         const struct timespec *abstime, int priv)
{
    if (clock_id != CLOCK_REALTIME && clock_id != CLOCK_MONOTONIC)
        return EINVAL;
    if (abstime && abstime->tv_sec < 0)
        return ETIMEDOUT;
    int op = fop(priv, FUTEX_WAIT_BITSET) | (clock_id == CLOCK_REALTIME ? FUTEX_CLOCK_REALTIME : 0);
    if (syscall(SYS_futex, addr, op, val, abstime, NULL, FUTEX_BITSET_MATCH_ANY) == -1)
        return errno;
    return 0;
}

int yolo_futex_unlock_pi(volatile int *addr, int priv)
{
    if (syscall(SYS_futex, addr, fop(priv, FUTEX_UNLOCK_PI), 0, NULL) == -1)
        return errno;
    return 0;
}

int yolo_futex_lock_pi(volatile int *addr, int priv, const struct timespec *abstime)
{
    if (abstime && abstime->tv_sec < 0)
        return ETIMEDOUT;
    if (syscall(SYS_futex, addr, fop(priv, FUTEX_LOCK_PI), 0, abstime) == -1)
        return errno;
    return 0;
}

void yolo_futex_requeue(volatile int *addr, int priv, int wake, int requeue, volatile int *addr2)
{
    syscall(SYS_futex, addr, fop(priv, FUTEX_REQUEUE), wake, requeue, addr2);
}

#include "compat_signals.inc"
