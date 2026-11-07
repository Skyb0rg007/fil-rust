/* mmap and what goes with it. Each function returns 0, or the line that went
   wrong, except for the ones that have to fail safely. */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/shm.h>
#include <sys/stat.h>
#include <unistd.h>

#define CHECK(condition) do { if (!(condition)) return __LINE__; } while (0)

int zsys_mincore(void* address, size_t length, char* result);

static const size_t page = 4096;

int anonymous_memory(void)
{
    char* memory = mmap(NULL, 3 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(memory != MAP_FAILED);
    CHECK(!((uintptr_t)memory % page));
    CHECK(memory[0] == 0 && memory[3 * page - 1] == 0);
    memset(memory, 'x', 3 * page);
    CHECK(mprotect(memory + page, page, PROT_READ) == 0);
    CHECK(memory[page] == 'x');
    CHECK(mprotect(memory + page, page, PROT_READ | PROT_WRITE) == 0);
    CHECK(mlock(memory, page) == 0 || errno == ENOMEM || errno == EPERM);
    CHECK(munlock(memory, page) == 0);
    unsigned char resident[3];
    /* musl's mincore can't get here: see docs/upstream-filc-issues.md. */
    CHECK(zsys_mincore(memory, 3 * page, (char*)resident) == 0);
    CHECK(msync(memory, page, MS_ASYNC) == 0);
    CHECK(madvise(memory, page, MADV_DONTNEED) == 0);
    CHECK(memory[0] == 0 && memory[page] == 'x');

    /* Pieces go back to zero, and the rest stays as it was. */
    CHECK(munmap(memory + page, page) == 0);
    CHECK(memory[page] == 0 && memory[2 * page] == 'x');
    CHECK(munmap(memory, 3 * page) == 0);
    return 0;
}

int rejected_arguments(void)
{
    char* memory = mmap(NULL, 2 * page, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(memory != MAP_FAILED);
    CHECK(mmap(NULL, page, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) == MAP_FAILED && errno == EINVAL);
    CHECK(mmap(NULL, page, PROT_READ, MAP_ANONYMOUS, -1, 0) == MAP_FAILED && errno == EINVAL);
    CHECK(mmap(memory, page, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) == MAP_FAILED && errno == EINVAL);
    CHECK(mmap(NULL, page, PROT_READ, MAP_PRIVATE, 12345, 0) == MAP_FAILED && errno == EBADF);
    CHECK(mprotect(memory, page, PROT_EXEC) == -1 && errno == EINVAL);
    CHECK(munmap(memory + 1, 1) == -1 && errno == EINVAL);
    CHECK(munmap(memory, 0) == 0);
    char* block = malloc(page);
    CHECK(mlock(block, page) == -1 && errno == ENOMEM);
    free(block);
    CHECK(munmap(memory, 2 * page) == 0);
    return 0;
}

int file_mappings(const char* path)
{
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    CHECK(fd >= 0);
    CHECK(ftruncate(fd, 2 * page) == 0);
    CHECK(pwrite(fd, "written", 8, page) == 8);
    char* shared = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(shared != MAP_FAILED);
    CHECK(!strcmp(shared + page, "written"));
    strcpy(shared, "through memory");
    char* again = mmap(NULL, page, PROT_READ, MAP_PRIVATE, fd, 0);
    CHECK(again != MAP_FAILED);
    CHECK(!strcmp(again, "through memory"));
    CHECK(munmap(again, page) == 0);

    /* A mapping can replace another one in place. */
    CHECK(mmap(shared, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == shared);
    CHECK(shared[0] == 0 && !strcmp(shared + page, "written"));

    char buffer[32];
    CHECK(pread(fd, buffer, 15, 0) == 15 && !strcmp(buffer, "through memory"));
    CHECK(close(fd) == 0);
    CHECK(munmap(shared, 2 * page) == 0);
    return 0;
}

int remapping(const char* path)
{
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    CHECK(fd >= 0);
    CHECK(ftruncate(fd, 4 * page) == 0);
    char* first = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(first != MAP_FAILED);
    strcpy(first, "kept");

    char* moved = mremap(first, 2 * page, 4 * page, MREMAP_MAYMOVE);
    CHECK(moved != MAP_FAILED);
    CHECK(moved != first);
    CHECK(!strcmp(moved, "kept"));
    moved[4 * page - 1] = 'y';

    /* The old one is still memory, with nothing of the file in it. */
    CHECK(first[0] == 0);

    /* Shrinking stays put, and growing in place can't work. */
    CHECK(mremap(moved, 4 * page, page, 0) == moved);
    CHECK(moved[page] == 0);
    CHECK(mremap(moved, page, 16 * page, 0) == MAP_FAILED && errno == ENOMEM);
    CHECK(mremap(moved, page, 0, MREMAP_MAYMOVE) == MAP_FAILED && errno == EINVAL);
    CHECK(mremap(moved + 1, page, page, MREMAP_MAYMOVE) == MAP_FAILED && errno == EINVAL);
    CHECK(close(fd) == 0);

    char* private = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(private != MAP_FAILED);
    CHECK(mremap(private, page, 2 * page, MREMAP_MAYMOVE) == MAP_FAILED && errno == ENOSYS);
    return 0;
}

int shared_memory(void)
{
    int id = shmget(IPC_PRIVATE, 2 * page, IPC_CREAT | 0600);
    CHECK(id >= 0);
    char* first = shmat(id, NULL, 0);
    CHECK(first != (void*)-1);
    char* second = shmat(id, NULL, 0);
    CHECK(second != (void*)-1);
    CHECK(first != second);
    strcpy(first, "seen");
    CHECK(!strcmp(second, "seen"));
    struct shmid_ds status;
    CHECK(shmctl(id, IPC_STAT, &status) == 0);
    CHECK(status.shm_segsz == 2 * page);
    CHECK(status.shm_nattch == 2);
    CHECK(shmdt(first) == 0);
    CHECK(shmctl(id, IPC_STAT, &status) == 0);
    CHECK(status.shm_nattch == 1);
    CHECK(shmat(id, second, 0) == (void*)-1 && errno == EINVAL);
    CHECK(shmdt(second) == 0);
    CHECK(shmctl(id, IPC_RMID, NULL) == 0);
    return 0;
}

/* Maps a file and forgets it. Only a collection can undo this, and that's what
   the caller wants to see. */
int forget_a_mapping(int fd)
{
    return mmap(NULL, 16 * page, PROT_READ, MAP_SHARED, fd, 0) == MAP_FAILED;
}

int free_a_mapping(void)
{
    free(mmap(NULL, page, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    return 0;
}

int use_after_munmap(void)
{
    char* memory = mmap(NULL, page, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    munmap(memory, page);
    return memory[0];
}

int unmap_past_the_end(void)
{
    char* memory = mmap(NULL, page, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    return munmap(memory, 2 * page);
}

int unmap_ordinary_memory(void)
{
    return munmap(aligned_alloc(4096, 4096), 4096);
}

int protect_ordinary_memory(void)
{
    return mprotect(aligned_alloc(4096, 4096), 4096, PROT_READ);
}

int drop_memory_outside_a_mapping(void)
{
    return madvise(aligned_alloc(4096, 4096), 4096, MADV_DONTNEED);
}

int map_over_ordinary_memory(void)
{
    return mmap(aligned_alloc(4096, 4096), 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == MAP_FAILED;
}

int detach_from_the_middle(void)
{
    char* memory = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    return shmdt(memory + page);
}

int unsupported_advice(void)
{
    char* memory = mmap(NULL, page, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    return madvise(memory, page, MADV_DONTFORK);
}
