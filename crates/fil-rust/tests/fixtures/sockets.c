/* Sockets. Each function returns 0, or the line that went wrong, except for the
   ones that have to fail safely. `pass_fd` and `sem_roundtrip`, at the end, are
   tests/abi_audit.rs's: they exercise libc paths whose structs differ between
   musl and glibc, and return a negative errno instead. */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/sem.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <unistd.h>

#define CHECK(condition) do { if (!(condition)) return __LINE__; } while (0)

int stream_pairs(void)
{
    int pair[2];
    char buffer[16] = {0};
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
    CHECK(send(pair[0], "hello", 6, 0) == 6);
    CHECK(recv(pair[1], buffer, sizeof(buffer), 0) == 6);
    CHECK(!strcmp(buffer, "hello"));

    /* A byte count past what's in the int is still reported. */
    CHECK(shutdown(pair[0], SHUT_WR) == 0);
    CHECK(recv(pair[1], buffer, sizeof(buffer), 0) == 0);
    CHECK(close(pair[0]) == 0 && close(pair[1]) == 0);
    return 0;
}

static void local_address(struct sockaddr_un* address, const char* path, const char* name)
{
    memset(address, 0, sizeof(*address));
    address->sun_family = AF_UNIX;
    snprintf(address->sun_path, sizeof(address->sun_path), "%s/%s", path, name);
}

int listening(const char* directory)
{
    struct sockaddr_un address, peer;
    socklen_t length = sizeof(peer);
    local_address(&address, directory, "listener");

    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(listener >= 0);
    CHECK(bind(listener, (struct sockaddr*)&address, sizeof(address)) == 0);
    CHECK(listen(listener, 4) == 0);
    CHECK(getsockname(listener, (struct sockaddr*)&peer, &length) == 0);
    CHECK(!strcmp(peer.sun_path, address.sun_path));
    CHECK(length > offsetof(struct sockaddr_un, sun_path));

    int client = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(client >= 0);
    CHECK(connect(client, (struct sockaddr*)&address, sizeof(address)) == 0);
    length = sizeof(peer);
    CHECK(getpeername(client, (struct sockaddr*)&peer, &length) == 0);
    CHECK(!strcmp(peer.sun_path, address.sun_path));

    length = sizeof(peer);
    int server = accept4(listener, (struct sockaddr*)&peer, &length, SOCK_CLOEXEC);
    CHECK(server >= 0);
    CHECK(fcntl(server, F_GETFD) & FD_CLOEXEC);
    CHECK(close(server) == 0);

    /* Asking for no address is fine, but an address needs its length. */
    int second = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(connect(second, (struct sockaddr*)&address, sizeof(address)) == 0);
    server = accept(listener, NULL, NULL);
    CHECK(server >= 0);
    CHECK(close(server) == 0);
    CHECK(accept(listener, (struct sockaddr*)&peer, NULL) == -1 && errno == EINVAL);

    CHECK(accept(12345, NULL, NULL) == -1 && errno == EBADF);
    CHECK(close(listener) == 0 && close(client) == 0 && close(second) == 0);
    return 0;
}

int datagrams(const char* directory)
{
    struct sockaddr_un here, there, from;
    local_address(&here, directory, "here");
    local_address(&there, directory, "there");
    int a = socket(AF_UNIX, SOCK_DGRAM, 0), b = socket(AF_UNIX, SOCK_DGRAM, 0);
    CHECK(a >= 0 && b >= 0);
    CHECK(bind(a, (struct sockaddr*)&here, sizeof(here)) == 0);
    CHECK(bind(b, (struct sockaddr*)&there, sizeof(there)) == 0);
    CHECK(sendto(a, "hi", 3, 0, (struct sockaddr*)&there, sizeof(there)) == 3);

    char buffer[8];
    socklen_t length = sizeof(from);
    CHECK(recvfrom(b, buffer, sizeof(buffer), 0, (struct sockaddr*)&from, &length) == 3);
    CHECK(!strcmp(buffer, "hi"));
    CHECK(!strcmp(from.sun_path, here.sun_path));
    CHECK(recvfrom(b, buffer, sizeof(buffer), MSG_DONTWAIT, (struct sockaddr*)&from, NULL) == -1 && errno == EINVAL);
    CHECK(recvfrom(b, buffer, sizeof(buffer), MSG_DONTWAIT, NULL, NULL) == -1 && errno == EAGAIN);
    CHECK(close(a) == 0 && close(b) == 0);
    return 0;
}

int options(void)
{
    int fd = socket(AF_UNIX, SOCK_STREAM, 0), value = 1 << 16, seen = 0;
    socklen_t length = sizeof(seen);
    CHECK(fd >= 0);
    CHECK(setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &value, sizeof(value)) == 0);
    CHECK(getsockopt(fd, SOL_SOCKET, SO_SNDBUF, &seen, &length) == 0);
    CHECK(length == sizeof(seen));
    CHECK(seen >= value);

    /* It reports how much it wrote, which can be less than there was room for. */
    long big = 0;
    length = sizeof(big);
    CHECK(getsockopt(fd, SOL_SOCKET, SO_TYPE, &big, &length) == 0);
    CHECK(length == sizeof(int));
    CHECK(big == SOCK_STREAM);
    CHECK(close(fd) == 0);
    return 0;
}

int messages(void)
{
    int pair[2];
    CHECK(socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) == 0);

    /* Two pieces out, and a descriptor along with them. */
    char first[] = "ab", second[] = "cde";
    struct iovec out[2] = {{first, 2}, {second, 3}};
    int file = open("/dev/null", O_RDONLY);
    CHECK(file >= 0);
    union {
        struct cmsghdr header;
        char bytes[CMSG_SPACE(sizeof(int))];
    } control;
    memset(&control, 0, sizeof(control));
    struct msghdr message;
    memset(&message, 0, sizeof(message));
    message.msg_iov = out;
    message.msg_iovlen = 2;
    message.msg_control = control.bytes;
    message.msg_controllen = sizeof(control.bytes);
    struct cmsghdr* header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &file, sizeof(int));
    CHECK(sendmsg(pair[0], &message, 0) == 5);

    /* Three pieces in, so the last has nothing, and the lengths come back. */
    char a[3] = {0}, b[8] = {0}, c[8] = {0};
    struct iovec in[3] = {{a, 3}, {b, 8}, {c, 8}};
    union {
        struct cmsghdr header;
        char bytes[CMSG_SPACE(sizeof(int)) + 16];
    } received;
    memset(&received, 0, sizeof(received));
    memset(&message, 0, sizeof(message));
    message.msg_iov = in;
    message.msg_iovlen = 3;
    message.msg_control = received.bytes;
    message.msg_controllen = sizeof(received.bytes);
    CHECK(recvmsg(pair[1], &message, 0) == 5);
    CHECK(!memcmp(a, "abc", 3) && !memcmp(b, "de", 2) && c[0] == 0);
    CHECK(message.msg_controllen == CMSG_SPACE(sizeof(int)));
    CHECK(message.msg_flags == 0);
    header = CMSG_FIRSTHDR(&message);
    CHECK(header && header->cmsg_type == SCM_RIGHTS);
    int passed;
    memcpy(&passed, CMSG_DATA(header), sizeof(int));
    CHECK(passed != file && fcntl(passed, F_GETFD) >= 0);
    CHECK(close(passed) == 0 && close(file) == 0);

    /* What doesn't fit is cut off, and it says so. */
    CHECK(send(pair[0], "abcdef", 6, 0) == 6);
    memset(&message, 0, sizeof(message));
    char small[2];
    struct iovec little = {small, 2};
    message.msg_iov = &little;
    message.msg_iovlen = 1;
    CHECK(recvmsg(pair[1], &message, 0) == 2);
    CHECK(message.msg_flags & MSG_TRUNC);
    CHECK(close(pair[0]) == 0 && close(pair[1]) == 0);
    return 0;
}

int message_names(const char* directory)
{
    struct sockaddr_un here, there, from;
    local_address(&here, directory, "named-here");
    local_address(&there, directory, "named-there");
    int a = socket(AF_UNIX, SOCK_DGRAM, 0), b = socket(AF_UNIX, SOCK_DGRAM, 0);
    CHECK(bind(a, (struct sockaddr*)&here, sizeof(here)) == 0);
    CHECK(bind(b, (struct sockaddr*)&there, sizeof(there)) == 0);

    char text[] = "named";
    struct iovec piece = {text, 6};
    struct msghdr message;
    memset(&message, 0, sizeof(message));
    message.msg_name = &there;
    message.msg_namelen = sizeof(there);
    message.msg_iov = &piece;
    message.msg_iovlen = 1;
    CHECK(sendmsg(a, &message, 0) == 6);

    char buffer[8];
    piece.iov_base = buffer;
    piece.iov_len = 8;
    memset(&message, 0, sizeof(message));
    memset(&from, 0, sizeof(from));
    message.msg_name = &from;
    message.msg_namelen = sizeof(from);
    message.msg_iov = &piece;
    message.msg_iovlen = 1;
    CHECK(recvmsg(b, &message, 0) == 6);
    CHECK(!strcmp(buffer, "named"));
    CHECK(!strcmp(from.sun_path, here.sun_path));
    CHECK(message.msg_namelen > offsetof(struct sockaddr_un, sun_path));
    CHECK(close(a) == 0 && close(b) == 0);
    return 0;
}

int many_messages(void)
{
    int pair[2];
    CHECK(socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) == 0);

    char one[] = "one", three[] = "three";
    struct iovec out[2] = {{one, 4}, {three, 6}};
    struct mmsghdr messages[2];
    memset(messages, 0, sizeof(messages));
    for (int i = 0; i < 2; i++) {
        messages[i].msg_hdr.msg_iov = &out[i];
        messages[i].msg_hdr.msg_iovlen = 1;
    }
    CHECK(sendmmsg(pair[0], messages, 2, 0) == 2);
    CHECK(messages[0].msg_len == 4 && messages[1].msg_len == 6);

    char a[8], b[8];
    struct iovec in[2] = {{a, 8}, {b, 8}};
    memset(messages, 0, sizeof(messages));
    for (int i = 0; i < 2; i++) {
        messages[i].msg_hdr.msg_iov = &in[i];
        messages[i].msg_hdr.msg_iovlen = 1;
    }
    struct timespec timeout = {1, 0};
    CHECK(recvmmsg(pair[1], messages, 2, 0, &timeout) == 2);
    CHECK(messages[0].msg_len == 4 && messages[1].msg_len == 6);
    CHECK(!strcmp(a, "one") && !strcmp(b, "three"));
    CHECK(close(pair[0]) == 0 && close(pair[1]) == 0);
    return 0;
}

static int pair_for_misuse[2];

static void make_pair(void)
{
    socketpair(AF_UNIX, SOCK_STREAM, 0, pair_for_misuse);
    send(pair_for_misuse[0], "0123456789", 10, 0);
}

int receive_past_the_end(void)
{
    make_pair();
    char* buffer = malloc(4);
    return recv(pair_for_misuse[1], buffer, 256, 0);
}

int receive_through_a_message(void)
{
    make_pair();
    char* buffer = malloc(4);
    struct iovec piece = {buffer, 256};
    struct msghdr message;
    memset(&message, 0, sizeof(message));
    message.msg_iov = &piece;
    message.msg_iovlen = 1;
    return recvmsg(pair_for_misuse[1], &message, 0);
}

int send_past_the_end(void)
{
    make_pair();
    char* buffer = malloc(4);
    return send(pair_for_misuse[0], buffer, 256, 0);
}

int message_with_too_many_pieces(void)
{
    make_pair();
    char buffer[4];
    struct iovec piece = {buffer, 4};
    struct msghdr message;
    memset(&message, 0, sizeof(message));
    message.msg_iov = &piece;
    message.msg_iovlen = 2;
    return recvmsg(pair_for_misuse[1], &message, 0);
}

int message_with_a_small_control_buffer(void)
{
    make_pair();
    char buffer[4], control[4];
    struct iovec piece = {buffer, 4};
    struct msghdr message;
    memset(&message, 0, sizeof(message));
    message.msg_iov = &piece;
    message.msg_iovlen = 1;
    message.msg_control = control;
    message.msg_controllen = 256;
    return recvmsg(pair_for_misuse[1], &message, 0);
}

int address_length_past_the_end(void)
{
    make_pair();
    char* address = malloc(4);
    socklen_t length = 256;
    return getsockname(pair_for_misuse[0], (struct sockaddr*)address, &length);
}

int received_address_past_the_end(void)
{
    make_pair();
    char buffer[10];
    char* address = malloc(4);
    socklen_t length = 256;
    return recvfrom(pair_for_misuse[1], buffer, sizeof(buffer), 0, (struct sockaddr*)address, &length);
}

int forged_pointer_in_a_message(void)
{
    make_pair();
    struct iovec piece = {(void*)0x1000, 8};
    struct msghdr message;
    memset(&message, 0, sizeof(message));
    message.msg_iov = &piece;
    message.msg_iovlen = 1;
    return recvmsg(pair_for_misuse[1], &message, 0);
}

int too_many_messages(void)
{
    make_pair();
    struct mmsghdr messages[1];
    memset(messages, 0, sizeof(messages));
    return recvmmsg(pair_for_misuse[1], messages, 2, 0, NULL);
}

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
