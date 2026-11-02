/* Stand-in for Fil-C's yolo musl (libyoloc.so) on top of the system glibc.
   Provides only the non-standard extras libpizlo imports; everything else
   resolves to glibc via NEEDED. */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stddef.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
#include <linux/futex.h>

/* glibc reserves SIGCANCEL (32) and SIGSETXID (33). */
int libc_internal_signals[] = { 32, 33 };
unsigned num_libc_internal_signals = 2;

int sigdelsetyolo(sigset_t *set, int sig)
{
    unsigned s = sig - 1;
    if (s >= 64) { errno = EINVAL; return -1; }
    unsigned long *bits = (unsigned long *)set;
    bits[s / (8 * sizeof(long))] &= ~(1UL << (s % (8 * sizeof(long))));
    return 0;
}

ssize_t getdents(int fd, void *buf, size_t len)
{
    return syscall(SYS_getdents64, fd, buf, len);
}

static int fop(int priv, int op) { return priv ? (op | FUTEX_PRIVATE_FLAG) : op; }

void yolo_futex_wake(volatile int *addr, int cnt, int priv)
{
    if (cnt < 0) cnt = 0x7fffffff;
    if (syscall(SYS_futex, addr, fop(priv, FUTEX_WAKE), cnt) == -1 && errno == ENOSYS)
        syscall(SYS_futex, addr, FUTEX_WAKE, cnt);
}

void yolo_futex_wait(volatile int *addr, int val, int priv)
{
    syscall(SYS_futex, addr, fop(priv, FUTEX_WAIT), val, NULL);
}

/* musl __timedwait semantics: absolute timeout on clock_id, returns positive errno. */
int yolo_futex_timedwait(volatile int *addr, int val, int clock_id,
                         const struct timespec *at, int priv)
{
    int op = fop(priv, FUTEX_WAIT_BITSET) | (clock_id == CLOCK_REALTIME ? FUTEX_CLOCK_REALTIME : 0);
    long r = syscall(SYS_futex, addr, op, val, at, NULL, FUTEX_BITSET_MATCH_ANY);
    if (r == -1) {
        int e = errno;
        if (e == EINTR || e == ETIMEDOUT || e == ECANCELED) return e;
    }
    return 0;
}

int yolo_futex_unlock_pi(volatile int *addr, int priv)
{
    return syscall(SYS_futex, addr, fop(priv, FUTEX_UNLOCK_PI)) == -1 ? -errno : 0;
}

int yolo_futex_lock_pi(volatile int *addr, int priv, const struct timespec *to)
{
    return syscall(SYS_futex, addr, fop(priv, FUTEX_LOCK_PI), 0, to) == -1 ? -errno : 0;
}

void yolo_futex_requeue(volatile int *addr, int priv, int wake, int requeue, volatile int *addr2)
{
    syscall(SYS_futex, addr, fop(priv, FUTEX_REQUEUE), wake, requeue, addr2);
}
