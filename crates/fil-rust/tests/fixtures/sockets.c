// Exercises libc paths whose structs differ between musl and glibc (see
// tests/abi_audit.rs): ancillary data, msghdr, and SysV IPC.
#include <errno.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/sem.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

/* Sends one end of a pipe over a socketpair with SCM_RIGHTS, writes through
   the received descriptor, and reads it back from the pipe. `fill` is the
   byte the control buffer is filled with before the cmsghdr is set up: musl
   programs that don't zero it leave cmsghdr's padding dirty. Returns 0, or
   a negative errno. */
int pass_fd(int fill)
{
    int sv[2], pipefd[2];
    if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sv) || pipe(pipefd))
        return -errno;

    union {
        char buf[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } control;
    memset(control.buf, fill, sizeof control.buf);
    char byte = 'x';
    struct iovec iov = { &byte, 1 };
    struct msghdr msg = { 0 };
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.buf;
    msg.msg_controllen = sizeof control.buf;
    struct cmsghdr* c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &pipefd[1], sizeof(int));
    if (sendmsg(sv[0], &msg, 0) != 1)
        return -errno;

    union {
        char buf[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } rcontrol;
    memset(rcontrol.buf, 0, sizeof rcontrol.buf);
    char rbyte = 0;
    struct iovec riov = { &rbyte, 1 };
    struct msghdr rmsg = { 0 };
    rmsg.msg_iov = &riov;
    rmsg.msg_iovlen = 1;
    rmsg.msg_control = rcontrol.buf;
    rmsg.msg_controllen = sizeof rcontrol.buf;
    if (recvmsg(sv[1], &rmsg, 0) != 1 || rbyte != 'x')
        return -errno - 1000;
    struct cmsghdr* rc = CMSG_FIRSTHDR(&rmsg);
    if (!rc || rc->cmsg_type != SCM_RIGHTS || rc->cmsg_len != CMSG_LEN(sizeof(int)))
        return -2000;
    int received;
    memcpy(&received, CMSG_DATA(rc), sizeof(int));
    if (write(received, "ok", 2) != 2)
        return -errno - 3000;
    char out[3] = { 0 };
    if (read(pipefd[0], out, 2) != 2 || strcmp(out, "ok"))
        return -4000;
    close(received);
    close(pipefd[0]);
    close(pipefd[1]);
    close(sv[0]);
    close(sv[1]);
    return 0;
}

/* Creates a semaphore set, checks IPC_STAT's sem_nsems (whose width
   differs), sets and reads a value, and removes the set. */
int sem_roundtrip(int nsems)
{
    int id = semget(IPC_PRIVATE, nsems, IPC_CREAT | 0600);
    if (id < 0)
        return -errno;
    struct semid_ds ds;
    memset(&ds, 0xff, sizeof ds);
    int result = 0;
    if (semctl(id, 0, IPC_STAT, &ds) < 0)
        result = -errno;
    else if ((int)ds.sem_nsems != nsems)
        result = -1000 - (int)ds.sem_nsems;
    else if (semctl(id, nsems - 1, SETVAL, 42) < 0)
        result = -errno;
    else if (semctl(id, nsems - 1, GETVAL) != 42)
        result = -2000;
    semctl(id, 0, IPC_RMID);
    return result;
}
