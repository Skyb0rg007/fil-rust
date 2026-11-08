/* The system calls behind the libc, one check after another. Each function
   returns 0, or the line that went wrong. */

#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <stdfil.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/random.h>
#include <sys/resource.h>
#include <sys/select.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <sys/utsname.h>
#include <time.h>
#include <unistd.h>

#define CHECK(condition) do { if (!(condition)) return __LINE__; } while (0)

int files(const char* directory)
{
    char path[512], other[512], link[512], buffer[64];
    snprintf(path, sizeof(path), "%s/file", directory);
    snprintf(other, sizeof(other), "%s/renamed", directory);
    snprintf(link, sizeof(link), "%s/link", directory);

    CHECK(chdir(directory) == 0);
    CHECK(getcwd(buffer, 1) == NULL && errno == ERANGE);
    char* cwd = getcwd(NULL, 0);
    CHECK(cwd && !strcmp(cwd, directory));
    free(cwd);

    CHECK(open("/does/not/exist", O_RDONLY) == -1 && errno == ENOENT);
    int fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0640);
    CHECK(fd >= 0);
    CHECK(write(fd, "hello world", 11) == 11);
    CHECK(lseek(fd, 6, SEEK_SET) == 6);
    CHECK(read(fd, buffer, sizeof(buffer)) == 5 && !memcmp(buffer, "world", 5));
    CHECK(pread(fd, buffer, 5, 0) == 5 && !memcmp(buffer, "hello", 5));
    CHECK(pwrite(fd, "J", 1, 0) == 1);

    struct stat by_fd, by_path;
    CHECK(fstat(fd, &by_fd) == 0);
    CHECK(by_fd.st_size == 11 && S_ISREG(by_fd.st_mode) && (by_fd.st_mode & 0777) == 0640);
    CHECK(stat(path, &by_path) == 0 && by_path.st_ino == by_fd.st_ino);
    CHECK(ftruncate(fd, 5) == 0);
    CHECK(fstat(fd, &by_fd) == 0 && by_fd.st_size == 5);
    CHECK(fchmod(fd, 0600) == 0);
    CHECK(fsync(fd) == 0);

    int flags = fcntl(fd, F_GETFL);
    CHECK(flags >= 0 && (flags & O_ACCMODE) == O_RDWR);
    CHECK(fcntl(fd, F_SETFD, FD_CLOEXEC) == 0 && fcntl(fd, F_GETFD) == FD_CLOEXEC);
    int copy = dup(fd);
    CHECK(copy >= 0 && copy != fd);
    CHECK(fcntl(copy, F_GETFD) == 0);
    CHECK(close(copy) == 0);
    CHECK(close(copy) == -1 && errno == EBADF);

    struct iovec out[2] = { { "ab", 2 }, { "cde", 3 } };
    CHECK(lseek(fd, 0, SEEK_SET) == 0);
    CHECK(writev(fd, out, 2) == 5);
    char first[2], second[3];
    struct iovec in[2] = { { first, 2 }, { second, 3 } };
    CHECK(lseek(fd, 0, SEEK_SET) == 0);
    CHECK(readv(fd, in, 2) == 5 && !memcmp(first, "ab", 2) && !memcmp(second, "cde", 3));
    CHECK(close(fd) == 0);

    CHECK(access(path, R_OK) == 0);
    CHECK(rename(path, other) == 0);
    CHECK(access(path, F_OK) == -1 && errno == ENOENT);
    CHECK(symlink(other, link) == 0);
    ssize_t length = readlink(link, buffer, sizeof(buffer));
    CHECK(length == (ssize_t)strlen(other) || length == sizeof(buffer));
    CHECK(lstat(link, &by_path) == 0 && S_ISLNK(by_path.st_mode));

    char sub[512];
    snprintf(sub, sizeof(sub), "%s/sub", directory);
    CHECK(mkdir(sub, 0755) == 0);
    CHECK(mkdir(sub, 0755) == -1 && errno == EEXIST);
    DIR* dir = opendir(directory);
    CHECK(dir);
    int seen = 0;
    for (struct dirent* entry; (entry = readdir(dir));)
        seen += !strcmp(entry->d_name, "renamed") + !strcmp(entry->d_name, "link") + !strcmp(entry->d_name, "sub");
    CHECK(seen == 3);
    CHECK(closedir(dir) == 0);
    CHECK(rmdir(sub) == 0);
    CHECK(unlink(link) == 0);
    CHECK(unlink(other) == 0);

    struct statvfs vfs;
    CHECK(statvfs(directory, &vfs) == 0 && vfs.f_bsize > 0);
    return 0;
}

int streams(void)
{
    int ends[2];
    CHECK(pipe(ends) == 0);
    struct pollfd poll_for = { .fd = ends[0], .events = POLLIN };
    CHECK(poll(&poll_for, 1, 0) == 0);
    fd_set readable;
    FD_ZERO(&readable);
    FD_SET(ends[0], &readable);
    struct timeval none = { 0, 0 };
    CHECK(select(ends[0] + 1, &readable, NULL, NULL, &none) == 0);

    CHECK(write(ends[1], "x", 1) == 1);
    CHECK(poll(&poll_for, 1, 1000) == 1 && (poll_for.revents & POLLIN));
    FD_ZERO(&readable);
    FD_SET(ends[0], &readable);
    CHECK(select(ends[0] + 1, &readable, NULL, NULL, NULL) == 1 && FD_ISSET(ends[0], &readable));

    int waiting = 0;
    CHECK(ioctl(ends[0], FIONREAD, &waiting) == 0 && waiting == 1);
    struct winsize size;
    CHECK(ioctl(ends[0], TIOCGWINSZ, &size) == -1 && errno == ENOTTY);
    CHECK(!isatty(ends[0]) && errno == ENOTTY);

    CHECK(close(ends[0]) == 0);
    CHECK(close(ends[1]) == 0);
    return 0;
}

/* A pointer in an event's data comes back as the same pointer, through a
   collection, and integers come back as they went. */
int epoll_events(void)
{
    int ends[2];
    CHECK(pipe(ends) == 0);
    int other[2];
    CHECK(pipe(other) == 0);
    int epoll = epoll_create1(EPOLL_CLOEXEC);
    CHECK(epoll >= 0);

    char* named = strdup("named by the event");
    struct epoll_event event = { .events = EPOLLIN, .data.ptr = named };
    CHECK(epoll_ctl(epoll, EPOLL_CTL_ADD, ends[0], &event) == 0);
    event = (struct epoll_event){ .events = EPOLLIN, .data.u64 = 0x1234567890abcdefull };
    CHECK(epoll_ctl(epoll, EPOLL_CTL_ADD, other[0], &event) == 0);

    struct epoll_event got[4];
    CHECK(epoll_wait(epoll, got, 4, 0) == 0);
    CHECK(epoll_wait(epoll, got, 0, 0) == -1 && errno == EINVAL);

    CHECK(write(ends[1], "x", 1) == 1);
    zgc_request_and_wait();
    memset(got, 0, sizeof(got));
    CHECK(epoll_wait(epoll, got, 4, 1000) == 1);
    CHECK(got[0].events == EPOLLIN && got[0].data.ptr == named);
    CHECK(!strcmp(got[0].data.ptr, "named by the event"));

    CHECK(write(other[1], "y", 1) == 1);
    CHECK(epoll_ctl(epoll, EPOLL_CTL_DEL, ends[0], NULL) == 0);
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR1);
    memset(got, 0, sizeof(got));
    CHECK(epoll_pwait(epoll, got, 4, 1000, &mask) == 1);
    CHECK(got[0].events == EPOLLIN && got[0].data.u64 == 0x1234567890abcdefull);

    event = (struct epoll_event){ .events = EPOLLOUT, .data.u64 = 7 };
    CHECK(epoll_ctl(epoll, EPOLL_CTL_MOD, other[0], &event) == 0);
    CHECK(epoll_wait(epoll, got, 4, 0) == 0);
    CHECK(epoll_ctl(epoll, EPOLL_CTL_ADD, other[0], &event) == -1 && errno == EEXIST);

    free(named);
    CHECK(close(epoll) == 0);
    for (int i = 0; i < 2; i++)
        CHECK(close(ends[i]) == 0 && close(other[i]) == 0);
    return 0;
}

int process_state(void)
{
    CHECK(getpid() > 0 && getppid() > 0);
    CHECK(getuid() == geteuid() && getgid() == getegid());

    struct timespec before, after, nap = { 0, 1000000 };
    CHECK(clock_gettime(CLOCK_MONOTONIC, &before) == 0);
    CHECK(nanosleep(&nap, NULL) == 0);
    CHECK(clock_gettime(CLOCK_MONOTONIC, &after) == 0);
    CHECK(after.tv_sec > before.tv_sec || after.tv_nsec > before.tv_nsec);
    CHECK(clock_getres(CLOCK_MONOTONIC, &before) == 0);
    CHECK(clock_gettime(-1, &before) == -1 && errno == EINVAL);

    struct utsname name;
    CHECK(uname(&name) == 0 && !strcmp(name.sysname, "Linux"));
    char random[16] = { 0 }, zeros[16] = { 0 };
    CHECK(getrandom(random, sizeof(random), 0) == sizeof(random) && memcmp(random, zeros, sizeof(zeros)));
    struct rlimit limit;
    CHECK(getrlimit(RLIMIT_NOFILE, &limit) == 0 && limit.rlim_cur > 0);
    struct rusage usage;
    CHECK(getrusage(RUSAGE_SELF, &usage) == 0);
    cpu_set_t cpus;
    CHECK(sched_getaffinity(0, sizeof(cpus), &cpus) == 0 && CPU_COUNT(&cpus) > 0);
    CHECK(sched_yield() == 0);
    CHECK(umask(umask(022)) == 022);
    return 0;
}

int dynamic_loading(const char* path)
{
    CHECK(dlopen("/does/not/exist.so", RTLD_NOW) == NULL);
    const char* message = dlerror();
    CHECK(message && strstr(message, "exist.so"));
    CHECK(dlerror() == NULL);

    void* library = dlopen(path, RTLD_NOW | RTLD_LOCAL);
    CHECK(library);
    int (*add)(int, int) = dlsym(library, "add");
    CHECK(add && add(2, 3) == 5);
    CHECK(dlsym(library, "not_there") == NULL);
    CHECK(dlerror());
    return 0;
}

int printed(void)
{
    char text[64];
    CHECK(snprintf(text, sizeof(text), "%d %s %.2f", 42, "text", 1.5) == 12);
    CHECK(!strcmp(text, "42 text 1.50"));
    FILE* file = tmpfile();
    CHECK(file);
    CHECK(fprintf(file, "line one\nline two\n") == 18);
    rewind(file);
    CHECK(fgets(text, sizeof(text), file) && !strcmp(text, "line one\n"));
    CHECK(fclose(file) == 0);
    return 0;
}

long read_into_small_buffer(int fd)
{
    char small[16];
    return read(fd, small, 64);
}
