/* Prints the layout of every libc struct, and the value of every libc
   constant, that libpizlo hands to the yolo libc. tests/abi_audit.rs builds
   it once against musl's headers and once against glibc's and compares: in
   the musl flavor, libpizlo was compiled against musl's headers but calls
   the host glibc. */
#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/file.h>
#include <sys/inotify.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/msg.h>
#include <sys/resource.h>
#include <sys/select.h>
#include <sys/sem.h>
#include <sys/shm.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/statvfs.h>
#include <sys/sysinfo.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <sys/times.h>
#include <sys/timex.h>
#include <sys/uio.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <time.h>
#include <unistd.h>

#define S(t) printf("sizeof %s %zu\n", #t, sizeof(t)); printf("alignof %s %zu\n", #t, _Alignof(t))
#define F(t, f) printf("field %s.%s %zu %zu\n", #t, #f, offsetof(t, f), sizeof(((t*)0)->f))
#define C(c) printf("const %s %lld\n", #c, (long long)(c))

int main(void)
{
    /* Files and directories. */
    S(struct stat); F(struct stat, st_dev); F(struct stat, st_ino); F(struct stat, st_nlink);
    F(struct stat, st_mode); F(struct stat, st_uid); F(struct stat, st_gid); F(struct stat, st_rdev);
    F(struct stat, st_size); F(struct stat, st_blksize); F(struct stat, st_blocks);
    F(struct stat, st_atim); F(struct stat, st_mtim); F(struct stat, st_ctim);
    S(struct statfs); F(struct statfs, f_type); F(struct statfs, f_bsize); F(struct statfs, f_blocks);
    F(struct statfs, f_bfree); F(struct statfs, f_bavail); F(struct statfs, f_files); F(struct statfs, f_ffree);
    F(struct statfs, f_fsid); F(struct statfs, f_namelen); F(struct statfs, f_frsize); F(struct statfs, f_flags);
    S(struct statvfs); F(struct statvfs, f_bsize); F(struct statvfs, f_frsize); F(struct statvfs, f_blocks);
    F(struct statvfs, f_bfree); F(struct statvfs, f_bavail); F(struct statvfs, f_files); F(struct statvfs, f_ffree);
    F(struct statvfs, f_favail); F(struct statvfs, f_fsid); F(struct statvfs, f_flag); F(struct statvfs, f_namemax);
    S(struct flock); F(struct flock, l_type); F(struct flock, l_whence); F(struct flock, l_start);
    F(struct flock, l_len); F(struct flock, l_pid);
    S(struct dirent); F(struct dirent, d_ino); F(struct dirent, d_off); F(struct dirent, d_reclen);
    F(struct dirent, d_type); F(struct dirent, d_name);
    S(struct file_handle); F(struct file_handle, handle_bytes); F(struct file_handle, handle_type);
    S(struct iovec); F(struct iovec, iov_base); F(struct iovec, iov_len);
    S(struct inotify_event);

    /* Time. */
    S(struct timespec); F(struct timespec, tv_sec); F(struct timespec, tv_nsec);
    S(struct timeval); F(struct timeval, tv_sec); F(struct timeval, tv_usec);
    S(struct itimerval); S(struct itimerspec); S(struct tms); S(time_t); S(clock_t);
    S(struct timex); F(struct timex, modes); F(struct timex, offset); F(struct timex, freq);
    F(struct timex, maxerror); F(struct timex, esterror); F(struct timex, status); F(struct timex, constant);
    F(struct timex, precision); F(struct timex, tolerance); F(struct timex, time); F(struct timex, tick);
    F(struct timex, ppsfreq); F(struct timex, jitter); F(struct timex, shift); F(struct timex, stabil);
    F(struct timex, jitcnt); F(struct timex, calcnt); F(struct timex, errcnt); F(struct timex, stbcnt);
    F(struct timex, tai);

    /* Processes and resources. */
    S(struct rlimit); S(struct rusage); F(struct rusage, ru_utime); F(struct rusage, ru_stime);
    F(struct rusage, ru_maxrss); F(struct rusage, ru_nivcsw);
    S(struct sysinfo); F(struct sysinfo, uptime); F(struct sysinfo, loads); F(struct sysinfo, totalram);
    F(struct sysinfo, procs); F(struct sysinfo, totalhigh); F(struct sysinfo, mem_unit);
    S(struct utsname); F(struct utsname, release); F(struct utsname, machine); F(struct utsname, domainname);
    S(struct sched_param); S(cpu_set_t); S(siginfo_t); S(id_t); S(pid_t); S(uid_t); S(gid_t); S(mode_t);
    S(off_t); S(dev_t); S(ino_t); S(nlink_t); S(blksize_t); S(blkcnt_t); S(socklen_t);

    /* Signals. */
    S(sigset_t); S(struct sigaction); F(struct sigaction, sa_handler); F(struct sigaction, sa_mask);
    F(struct sigaction, sa_flags); S(stack_t); S(union sigval); S(struct signalfd_siginfo);
    F(siginfo_t, si_signo); F(siginfo_t, si_errno); F(siginfo_t, si_code);

    /* Threads. */
    S(pthread_t); S(pthread_attr_t); S(pthread_mutex_t); S(pthread_mutexattr_t); S(pthread_cond_t);
    S(pthread_condattr_t); S(pthread_key_t); S(pthread_once_t);

    /* Sockets and polling. */
    S(struct msghdr); F(struct msghdr, msg_name); F(struct msghdr, msg_namelen); F(struct msghdr, msg_iov);
    F(struct msghdr, msg_iovlen); F(struct msghdr, msg_control); F(struct msghdr, msg_controllen);
    F(struct msghdr, msg_flags);
    S(struct cmsghdr); F(struct cmsghdr, cmsg_len); F(struct cmsghdr, cmsg_level); F(struct cmsghdr, cmsg_type);
    S(struct mmsghdr); F(struct mmsghdr, msg_hdr); F(struct mmsghdr, msg_len);
    S(struct sockaddr); S(struct sockaddr_storage); S(struct linger); S(struct ucred);
    S(struct epoll_event); F(struct epoll_event, events); F(struct epoll_event, data);
    S(struct pollfd); S(fd_set); S(nfds_t);

    /* SysV IPC. */
    S(struct ipc_perm); F(struct ipc_perm, uid); F(struct ipc_perm, gid); F(struct ipc_perm, cuid);
    F(struct ipc_perm, cgid); F(struct ipc_perm, mode);
    S(struct msqid_ds); F(struct msqid_ds, msg_perm); F(struct msqid_ds, msg_stime); F(struct msqid_ds, msg_rtime);
    F(struct msqid_ds, msg_ctime); F(struct msqid_ds, msg_qnum); F(struct msqid_ds, msg_qbytes);
    F(struct msqid_ds, msg_lspid); F(struct msqid_ds, msg_lrpid);
    S(struct semid_ds); F(struct semid_ds, sem_perm); F(struct semid_ds, sem_otime); F(struct semid_ds, sem_ctime);
    F(struct semid_ds, sem_nsems);
    S(struct shmid_ds); F(struct shmid_ds, shm_perm); F(struct shmid_ds, shm_segsz); F(struct shmid_ds, shm_atime);
    F(struct shmid_ds, shm_dtime); F(struct shmid_ds, shm_ctime); F(struct shmid_ds, shm_cpid);
    F(struct shmid_ds, shm_lpid); F(struct shmid_ds, shm_nattch);
    S(struct sembuf); F(struct sembuf, sem_num); F(struct sembuf, sem_op); F(struct sembuf, sem_flg);

    /* Dynamic loading. */
    S(Dl_info); F(Dl_info, dli_fname); F(Dl_info, dli_fbase); F(Dl_info, dli_sname); F(Dl_info, dli_saddr);

    /* Constants passed through unchanged. */
    C(O_RDONLY); C(O_WRONLY); C(O_RDWR); C(O_CREAT); C(O_EXCL); C(O_NOCTTY); C(O_TRUNC); C(O_APPEND);
    C(O_NONBLOCK); C(O_DSYNC); C(O_SYNC); C(O_RSYNC); C(O_DIRECTORY); C(O_NOFOLLOW); C(O_CLOEXEC);
    C(O_ASYNC); C(O_DIRECT); C(O_LARGEFILE); C(O_NOATIME); C(O_PATH); C(O_TMPFILE); C(O_NDELAY);
    C(F_DUPFD); C(F_GETFD); C(F_SETFD); C(F_GETFL); C(F_SETFL); C(F_GETLK); C(F_SETLK); C(F_SETLKW);
    C(F_SETOWN); C(F_GETOWN); C(F_SETSIG); C(F_GETSIG); C(F_DUPFD_CLOEXEC); C(F_SETPIPE_SZ); C(F_GETPIPE_SZ);
    C(F_OFD_GETLK); C(F_OFD_SETLK); C(F_OFD_SETLKW); C(F_ADD_SEALS); C(F_GET_SEALS); C(FD_CLOEXEC);
    C(F_RDLCK); C(F_WRLCK); C(F_UNLCK); C(LOCK_SH); C(LOCK_EX); C(LOCK_NB); C(LOCK_UN);
    C(AT_FDCWD); C(AT_SYMLINK_NOFOLLOW); C(AT_REMOVEDIR); C(AT_SYMLINK_FOLLOW); C(AT_EACCESS);
    C(AT_EMPTY_PATH); C(AT_NO_AUTOMOUNT);
    C(S_IFMT); C(S_IFDIR); C(S_IFREG); C(S_IFLNK); C(S_IFIFO); C(S_IFSOCK); C(S_IFCHR); C(S_IFBLK);
    C(SEEK_SET); C(SEEK_CUR); C(SEEK_END); C(SEEK_DATA); C(SEEK_HOLE);
    C(R_OK); C(W_OK); C(X_OK); C(F_OK);
    C(PROT_READ); C(PROT_WRITE); C(PROT_EXEC); C(PROT_NONE); C(MAP_SHARED); C(MAP_PRIVATE); C(MAP_FIXED);
    C(MAP_ANONYMOUS); C(MAP_NORESERVE); C(MAP_POPULATE); C(MAP_GROWSDOWN); C(MAP_STACK); C(MAP_FIXED_NOREPLACE);
    C(MS_ASYNC); C(MS_SYNC); C(MS_INVALIDATE); C(MADV_DONTNEED); C(MADV_WILLNEED); C(MADV_FREE);
    C(MREMAP_MAYMOVE); C(MREMAP_FIXED); C(MCL_CURRENT); C(MCL_FUTURE); C(MFD_CLOEXEC); C(MFD_ALLOW_SEALING);
    C(_SC_PAGESIZE); C(_SC_NPROCESSORS_ONLN); C(_SC_NPROCESSORS_CONF); C(_SC_CLK_TCK); C(_SC_OPEN_MAX);
    C(_SC_ARG_MAX); C(_SC_PHYS_PAGES); C(_SC_GETPW_R_SIZE_MAX); C(_SC_GETGR_R_SIZE_MAX); C(_SC_HOST_NAME_MAX);
    C(_SC_LINE_MAX); C(_SC_NGROUPS_MAX); C(_SC_CHILD_MAX); C(_SC_IOV_MAX);
    C(_PC_LINK_MAX); C(_PC_NAME_MAX); C(_PC_PATH_MAX); C(_PC_PIPE_BUF);
    C(SIGHUP); C(SIGINT); C(SIGQUIT); C(SIGILL); C(SIGTRAP); C(SIGABRT); C(SIGBUS); C(SIGFPE); C(SIGKILL);
    C(SIGUSR1); C(SIGSEGV); C(SIGUSR2); C(SIGPIPE); C(SIGALRM); C(SIGTERM); C(SIGSTKFLT); C(SIGCHLD);
    C(SIGCONT); C(SIGSTOP); C(SIGTSTP); C(SIGTTIN); C(SIGTTOU); C(SIGURG); C(SIGXCPU); C(SIGXFSZ);
    C(SIGVTALRM); C(SIGPROF); C(SIGWINCH); C(SIGIO); C(SIGPWR); C(SIGSYS); C(SIGRTMAX); C(_NSIG);
    C(SA_NOCLDSTOP); C(SA_NOCLDWAIT); C(SA_SIGINFO); C(SA_ONSTACK); C(SA_RESTART); C(SA_NODEFER);
    C(SA_RESETHAND); C(SIG_BLOCK); C(SIG_UNBLOCK); C(SIG_SETMASK); C(SS_ONSTACK); C(SS_DISABLE);
    C(MINSIGSTKSZ); C(SI_USER); C(SI_QUEUE); C(SI_TIMER);
    C(WNOHANG); C(WUNTRACED); C(WCONTINUED); C(WEXITED); C(WSTOPPED); C(WNOWAIT); C(P_ALL); C(P_PID); C(P_PGID);
    C(CLOCK_REALTIME); C(CLOCK_MONOTONIC); C(CLOCK_PROCESS_CPUTIME_ID); C(CLOCK_THREAD_CPUTIME_ID);
    C(CLOCK_MONOTONIC_RAW); C(CLOCK_REALTIME_COARSE); C(CLOCK_MONOTONIC_COARSE); C(CLOCK_BOOTTIME);
    C(TIMER_ABSTIME); C(ITIMER_REAL); C(ITIMER_VIRTUAL); C(ITIMER_PROF); C(TFD_CLOEXEC); C(TFD_NONBLOCK);
    C(TFD_TIMER_ABSTIME); C(ADJ_OFFSET); C(ADJ_FREQUENCY); C(ADJ_SETOFFSET);
    C(RLIMIT_CPU); C(RLIMIT_FSIZE); C(RLIMIT_DATA); C(RLIMIT_STACK); C(RLIMIT_CORE); C(RLIMIT_NOFILE);
    C(RLIMIT_AS); C(RLIMIT_NPROC); C(RLIMIT_MEMLOCK); C(RLIM_INFINITY); C(RUSAGE_SELF); C(RUSAGE_CHILDREN);
    C(RUSAGE_THREAD); C(PRIO_PROCESS); C(PRIO_PGRP); C(PRIO_USER);
    C(SCHED_OTHER); C(SCHED_FIFO); C(SCHED_RR); C(SCHED_BATCH); C(SCHED_IDLE);
    C(CLONE_NEWNS); C(CLONE_NEWUTS); C(CLONE_NEWIPC); C(CLONE_NEWUSER); C(CLONE_NEWPID); C(CLONE_NEWNET);
    C(AF_UNIX); C(AF_INET); C(AF_INET6); C(AF_NETLINK); C(AF_PACKET); C(SOCK_STREAM); C(SOCK_DGRAM);
    C(SOCK_RAW); C(SOCK_SEQPACKET); C(SOCK_NONBLOCK); C(SOCK_CLOEXEC); C(SOL_SOCKET); C(SO_REUSEADDR);
    C(SO_KEEPALIVE); C(SO_BROADCAST); C(SO_LINGER); C(SO_RCVBUF); C(SO_SNDBUF); C(SO_RCVTIMEO); C(SO_SNDTIMEO);
    C(SO_ERROR); C(SO_TYPE); C(SO_PEERCRED); C(SO_REUSEPORT); C(SO_PASSCRED); C(SO_DOMAIN); C(SO_PROTOCOL);
    C(SCM_RIGHTS); C(SCM_CREDENTIALS); C(MSG_OOB); C(MSG_PEEK); C(MSG_DONTWAIT); C(MSG_WAITALL);
    C(MSG_NOSIGNAL); C(MSG_TRUNC); C(MSG_CTRUNC); C(MSG_CMSG_CLOEXEC); C(MSG_EOR); C(MSG_MORE);
    C(SHUT_RD); C(SHUT_WR); C(SHUT_RDWR);
    C(POLLIN); C(POLLPRI); C(POLLOUT); C(POLLERR); C(POLLHUP); C(POLLNVAL); C(POLLRDHUP);
    C(EPOLLIN); C(EPOLLOUT); C(EPOLLERR); C(EPOLLHUP); C(EPOLLRDHUP); C(EPOLLET); C(EPOLLONESHOT);
    C(EPOLLEXCLUSIVE); C(EPOLL_CTL_ADD); C(EPOLL_CTL_DEL); C(EPOLL_CTL_MOD); C(EPOLL_CLOEXEC);
    C(EFD_CLOEXEC); C(EFD_NONBLOCK); C(EFD_SEMAPHORE); C(SFD_CLOEXEC); C(SFD_NONBLOCK);
    C(IN_CLOEXEC); C(IN_NONBLOCK); C(IN_MODIFY); C(IN_CREATE); C(IN_DELETE); C(IN_ALL_EVENTS);
    C(IPC_CREAT); C(IPC_EXCL); C(IPC_NOWAIT); C(IPC_PRIVATE); C(IPC_RMID); C(IPC_SET); C(IPC_STAT);
    C(IPC_INFO); C(MSG_NOERROR); C(MSG_EXCEPT); C(MSG_STAT); C(MSG_INFO); C(SEM_UNDO); C(GETPID); C(GETVAL);
    C(GETALL); C(GETNCNT); C(GETZCNT); C(SETVAL); C(SETALL); C(SEM_STAT); C(SEM_INFO); C(SHM_RDONLY);
    C(SHM_RND); C(SHM_LOCK); C(SHM_UNLOCK); C(SHM_STAT); C(SHM_INFO);
    C(XATTR_CREATE); C(XATTR_REPLACE); C(RTLD_LAZY); C(RTLD_NOW); C(RTLD_GLOBAL); C(RTLD_LOCAL);
    C(RTLD_NODELETE); C(RTLD_NOLOAD); C(PTHREAD_STACK_MIN);
    C(EPERM); C(ENOENT); C(EINTR); C(EAGAIN); C(EWOULDBLOCK); C(ENOMEM); C(EACCES); C(EFAULT); C(EEXIST);
    C(EINVAL); C(ENOSYS); C(ENOTSUP); C(EOPNOTSUPP); C(ETIMEDOUT); C(EDEADLK); C(ENAMETOOLONG); C(ENOTEMPTY);
    C(ELOOP); C(ENODATA); C(EOVERFLOW); C(EILSEQ); C(ECONNREFUSED); C(EINPROGRESS); C(EALREADY);
    C(EADDRINUSE); C(ECANCELED); C(EOWNERDEAD); C(ENOTRECOVERABLE); C(ERANGE); C(EPIPE); C(ESRCH);
    return 0;
}
