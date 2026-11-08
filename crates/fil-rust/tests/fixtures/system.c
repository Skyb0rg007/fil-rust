/* Processes, System V IPC, timers and what's left of the system calls. Each
   function returns 0, or the line that went wrong, except for the ones that
   have to fail safely. */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/openat2.h>
#include <pizlonated_syscalls.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdfil.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/ipc.h>
#include <sys/mount.h>
#include <sys/msg.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/sem.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(condition) do { if (!(condition)) return __LINE__; } while (0)

/* What a child reports, through its exit status. */
#define CHILD_CHECK(condition) do { if (!(condition)) _exit(1); } while (0)

static void* churn(void* stop)
{
    while (!__atomic_load_n((int*)stop, __ATOMIC_RELAXED))
        free(malloc(128));
    return NULL;
}

static int wait_for(pid_t pid)
{
    int status = 0;
    if (waitpid(pid, &status, 0) != pid)
        return -1;
    return WIFEXITED(status) ? WEXITSTATUS(status) : -2;
}

static void* quickly(void* value)
{
    return value;
}

/* The child has a runtime of its own, with one thread in it. */
int fork_from_many_threads(int rounds)
{
    int stop = 0;
    pthread_t workers[4];
    for (int i = 0; i < 4; i++)
        CHECK(!pthread_create(&workers[i], NULL, churn, &stop));
    for (int round = 0; round < rounds; round++) {
        pid_t pid = fork();
        CHECK(pid >= 0);
        if (!pid) {
            CHILD_CHECK(getpid() == gettid());
            CHILD_CHECK(zthread_self_id() == (unsigned)gettid());
            char* memory = malloc(1 << 16);
            CHILD_CHECK(memory);
            memset(memory, 1, 1 << 16);
            zgc_request_and_wait();
            CHILD_CHECK(memory[1 << 15] == 1);
            pthread_t thread;
            void* result = NULL;
            CHILD_CHECK(!pthread_create(&thread, NULL, quickly, memory));
            CHILD_CHECK(!pthread_join(thread, &result));
            CHILD_CHECK(result == memory);
            zgc_request_and_wait();
            _exit(9);
        }
        CHECK(wait_for(pid) == 9);
    }
    stop = 1;
    for (int i = 0; i < 4; i++)
        CHECK(!pthread_join(workers[i], NULL));
    return 0;
}

int waiting_for_children(void)
{
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (!pid)
        _exit(3);
    int status = 0;
    struct rusage usage;
    CHECK(wait4(pid, &status, 0, &usage) == pid);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 3);

    pid = fork();
    CHECK(pid >= 0);
    if (!pid)
        _exit(4);
    siginfo_t info;
    memset(&info, 0, sizeof(info));
    CHECK(waitid(P_PID, pid, &info, WEXITED) == 0);
    CHECK(info.si_pid == pid && info.si_code == CLD_EXITED && info.si_status == 4);

    pid = fork();
    CHECK(pid >= 0);
    if (!pid) {
        raise(SIGKILL);
        _exit(0);
    }
    CHECK(waitpid(pid, &status, 0) == pid);
    CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);

    CHECK(waitpid(pid, NULL, WNOHANG) == -1 && errno == ECHILD);
    return 0;
}

int starting_programs(const char* path)
{
    char* arguments[] = {(char*)path, "--exact", "child_entry", "--nocapture", NULL};
    char* environment[] = {"FIL_RUST_CHILD=exec-reached", NULL};
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (!pid) {
        execve(path, arguments, environment);
        _exit(1);
    }
    CHECK(wait_for(pid) == 42);

    pid = fork();
    CHECK(pid >= 0);
    if (!pid) {
        /* musl's fexecve can't get here: see docs/upstream-filc-issues.md. */
        int fd = open(path, O_RDONLY);
        zsys_fexecve(fd, arguments, environment);
        _exit(1);
    }
    CHECK(wait_for(pid) == 42);

    CHECK(execve("/nonexistent/program", arguments, environment) == -1 && errno == ENOENT);
    return 0;
}

int shared_message_queues(void)
{
    int queue = msgget(IPC_PRIVATE, IPC_CREAT | 0600);
    CHECK(queue >= 0);
    struct {
        long type;
        char text[16];
    } out = {7, "queued"}, in;
    CHECK(msgsnd(queue, &out, 7, 0) == 0);
    struct msqid_ds status;
    CHECK(msgctl(queue, IPC_STAT, &status) == 0);
    CHECK(status.msg_qnum == 1);
    memset(&in, 0, sizeof(in));
    CHECK(msgrcv(queue, &in, sizeof(in.text), 7, 0) == 7);
    CHECK(in.type == 7 && !strcmp(in.text, "queued"));
    CHECK(msgrcv(queue, &in, sizeof(in.text), 0, IPC_NOWAIT) == -1 && errno == ENOMSG);
    CHECK(msgctl(queue, IPC_RMID, NULL) == 0);
    return 0;
}

union semaphore_argument {
    int value;
    struct semid_ds* status;
};

int semaphores(void)
{
    int set = semget(IPC_PRIVATE, 2, IPC_CREAT | 0600);
    CHECK(set >= 0);
    union semaphore_argument argument = {.value = 3};
    CHECK(semctl(set, 1, SETVAL, argument) == 0);
    CHECK(semctl(set, 1, GETVAL) == 3);
    struct sembuf operations[2] = {{1, -2, 0}, {0, 1, 0}};
    CHECK(semop(set, operations, 2) == 0);
    CHECK(semctl(set, 1, GETVAL) == 1);
    CHECK(semctl(set, 0, GETVAL) == 1);
    struct sembuf blocked = {1, -5, IPC_NOWAIT};
    CHECK(semop(set, &blocked, 1) == -1 && errno == EAGAIN);
    struct timespec soon = {0, 1000};
    blocked.sem_flg = 0;
    CHECK(semtimedop(set, &blocked, 1, &soon) == -1 && errno == EAGAIN);
    struct semid_ds status;
    argument.status = &status;
    CHECK(semctl(set, 0, IPC_STAT, argument) == 0);
    CHECK(status.sem_nsems == 2);
    CHECK(semctl(set, 0, 12345) == -1 && errno == EINVAL);
    CHECK(semctl(set, 0, IPC_RMID) == 0);
    return 0;
}

int timers(void)
{
    timer_t timer;
    struct sigevent event;
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_NONE;
    CHECK(timer_create(CLOCK_MONOTONIC, &event, &timer) == 0);
    struct itimerspec soon = {{0, 0}, {100, 0}}, seen;
    CHECK(timer_settime(timer, 0, &soon, NULL) == 0);
    CHECK(timer_gettime(timer, &seen) == 0);
    CHECK(seen.it_value.tv_sec > 90 && seen.it_value.tv_sec <= 100);
    CHECK(timer_getoverrun(timer) == 0);
    CHECK(timer_delete(timer) == 0);

    /* alarm reports what was left of the last one. */
    CHECK(alarm(1000) == 0);
    CHECK(alarm(0) >= 999);
    return 0;
}

int files(const char* directory)
{
    char path[512], other[512];
    snprintf(path, sizeof(path), "%s/made", directory);
    snprintf(other, sizeof(other), "%s/moved", directory);

    CHECK(mkdir(path, 0700) == 0);
    CHECK(mkdir(path, 0700) == -1 && errno == EEXIST);
    CHECK(access(path, R_OK) == 0);
    CHECK(access("/nonexistent/file", R_OK) == -1 && errno == ENOENT);
    CHECK(zsys_renameat2(AT_FDCWD, path, AT_FDCWD, other, 0) == 0);
    CHECK(access(other, F_OK) == 0 && access(path, F_OK) == -1);
    struct timeval times[2] = {{1000, 0}, {2000, 0}};
    CHECK(utimes(other, times) == 0);
    struct stat status;
    CHECK(stat(other, &status) == 0);
    CHECK(status.st_mtime == 2000);
    CHECK(lutimes(other, times) == 0);

    char buffer[256];
    memset(buffer, 0, sizeof(buffer));
    CHECK(zsys_statx(AT_FDCWD, other, 0, 0xfff, buffer) == 0);

    struct open_how how;
    memset(&how, 0, sizeof(how));
    how.flags = O_RDONLY;
    int fd = zsys_openat2(AT_FDCWD, other, &how, sizeof(how));
    CHECK(fd >= 0);
    CHECK(futimes(fd, times) == 0);
    CHECK(fpathconf(fd, _PC_NAME_MAX) == 255);
    CHECK(pathconf(other, _PC_NAME_MAX) == 255);
    CHECK(close(fd) == 0);

    /* Trailing zeroes are fine, and anything else might mean something. */
    char longer[sizeof(how) + 8];
    memset(longer, 0, sizeof(longer));
    fd = zsys_openat2(AT_FDCWD, other, longer, sizeof(longer));
    CHECK(fd >= 0);
    CHECK(close(fd) == 0);
    longer[sizeof(how)] = 1;
    CHECK(zsys_openat2(AT_FDCWD, other, longer, sizeof(longer)) == -1 && errno == E2BIG);

    fd = inotify_init();
    CHECK(fd >= 0);
    CHECK(close(fd) == 0);
    return 0;
}

int the_process(void)
{
    char name[16];
    CHECK(prctl(PR_SET_NAME, "fil-rust-test") == 0);
    CHECK(prctl(PR_GET_NAME, name) == 0);
    CHECK(!strcmp(name, "fil-rust-test"));
    CHECK(prctl(PR_GET_DUMPABLE) >= 0);
    int signal_number = -1;
    CHECK(prctl(PR_GET_PDEATHSIG, &signal_number) == 0);
    CHECK(signal_number == 0);
    CHECK(prctl(12345) == -1 && errno == ENOSYS);

    struct timeval now;
    struct timezone zone;
    CHECK(gettimeofday(&now, &zone) == 0);
    CHECK(now.tv_sec > 1000000000);

    char host[256];
    CHECK(gethostname(host, sizeof(host)) == 0);
    CHECK(getdomainname(host, sizeof(host)) == 0);

    unsigned long affinity[16];
    CHECK(zsys_raw_sched_getaffinity(0, sizeof(affinity), affinity) > 0);

    struct {
        unsigned version;
        int pid;
    } header = {0x20080522, 0};
    unsigned data[6];
    CHECK(zsys_capget(&header, data) == 0);
    header.version = 1;
    CHECK(zsys_capget(&header, data) == 0);
    CHECK(header.version == 0x20080522);

    int pidfd = zsys_pidfd_open(getpid(), 0);
    CHECK(pidfd >= 0 || errno == ENOSYS);
    if (pidfd >= 0)
        CHECK(close(pidfd) == 0);

    Dl_info info;
    CHECK(zsys_dladdr((void*)the_process, &info) != 0);
    CHECK(info.dli_fname && strstr(info.dli_fname, "system"));
    CHECK(info.dli_sname == NULL && info.dli_saddr == NULL);
    CHECK(info.dli_fbase != NULL);
    return 0;
}

/* Nothing here should work, as long as it's for the right reason: these go wrong
   on the path, or for lack of privilege, not on the arguments. */
int calls_that_need_privilege(void)
{
#define REFUSED(call) do { errno = 0; CHECK((call) < 0 && errno != EFAULT && errno != 0); } while (0)
    REFUSED(zsys_swapoff("/nonexistent/swap"));
    REFUSED(zsys_pivot_root("/nonexistent/new", "/nonexistent/old"));
    REFUSED(mount("/nonexistent/source", "/nonexistent/target", "tmpfs", 0, "size=1"));
    REFUSED(mount(NULL, "/nonexistent/target", NULL, MS_REMOUNT, NULL));
    return 0;
}

/* Upstream panics on this: see docs/upstream-filc-issues.md. */
int asking_for_a_quota(void)
{
    errno = 0;
    CHECK(zsys_quotactl(0x80000700, "/nonexistent/device", 0, (char[72]){0}) < 0);
    CHECK(errno != EFAULT && errno != 0);
    return 0;
}

int vector_splicing(void)
{
    int pipes[2];
    CHECK(pipe(pipes) == 0);
    char out[] = "spliced", in[8] = {0};
    struct iovec piece = {out, 8};
    CHECK(vmsplice(pipes[1], &piece, 1, 0) == 8);
    piece.iov_base = in;
    CHECK(vmsplice(pipes[0], &piece, 1, 0) == 8);
    CHECK(!strcmp(in, "spliced"));
    CHECK(close(pipes[0]) == 0 && close(pipes[1]) == 0);
    return 0;
}

static int always_zero;

int unterminated_arguments(void)
{
    char* arguments[2] = {"one", "two"};
    return execve("/nonexistent/program", arguments, NULL);
}

int bad_string_in_arguments(void)
{
    char* string = malloc(8);
    memset(string, 'x', 8);
    free(string);
    char* arguments[2] = {string, NULL};
    return execve("/nonexistent/program", arguments, NULL);
}

int short_message_buffer(void)
{
    int queue = msgget(IPC_PRIVATE, IPC_CREAT | 0600);
    char* message = malloc(16);
    int result = msgrcv(queue, message, 256, 0, IPC_NOWAIT);
    return result;
}

int short_wait_status(void)
{
    char* status = malloc(1);
    free(status);
    return waitpid(-1, (int*)status, WNOHANG);
}

int short_semaphore_operations(void)
{
    struct sembuf operation = {0, 1, 0};
    return semop(1, &operation, 100);
}

int mapping_tls_away(void)
{
    return zsys_arch_prctl(0x1002, (void*)0x1000);
}

int seccomp_in_strict_mode_is_not_for_fil_c(void)
{
    return always_zero ? 0 : prctl(PR_SET_SECCOMP, 1);
}
