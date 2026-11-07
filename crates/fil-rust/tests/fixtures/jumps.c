/* setjmp and longjmp, as a program uses them. */

#include <setjmp.h>
#include <signal.h>
#include <stdlib.h>

static void go_down(jmp_buf buffer, int depth, int value)
{
    if (!depth)
        longjmp(buffer, value);
    go_down(buffer, depth - 1, value);
}

int jump_up(int depth, int value)
{
    jmp_buf buffer;
    int result = setjmp(buffer);
    if (result)
        return result;
    go_down(buffer, depth, value);
    return -1;
}

/* Taken from the C standard: a zero becomes a one. */
int jump_with_zero(void)
{
    jmp_buf buffer;
    int result = setjmp(buffer);
    if (result)
        return result;
    longjmp(buffer, 0);
}

/* Changed after the setjmp, so C doesn't say which of the two it sees. Both are
   live objects, and that's what Fil-C promises. */
int pointer_changed_after_setjmp(void (*collect)(void))
{
    int* pointer = malloc(sizeof(int));
    *pointer = 1;
    jmp_buf buffer;
    if (!setjmp(buffer)) {
        pointer = malloc(sizeof(int));
        *pointer = 2;
        collect();
        go_down(buffer, 3, 1);
    }
    collect();
    return *pointer;
}

/* The buffer's the only thing that remembers what the frame held. */
int jump_after_collection(void (*collect)(void))
{
    int* first = malloc(sizeof(int));
    *first = 40;
    int* second = malloc(sizeof(int));
    *second = 2;
    jmp_buf buffer;
    if (setjmp(buffer))
        return *first + *second;
    collect();
    go_down(buffer, 3, 1);
    return -1;
}

long jump_in_a_loop(long times)
{
    jmp_buf buffer;
    volatile long count = 0;
    while (count < times) {
        if (!setjmp(buffer))
            longjmp(buffer, 1);
        count++;
    }
    return count;
}

static jmp_buf gone;

static void leave_a_buffer(void)
{
    setjmp(gone);
}

void jump_to_a_returned_frame(void)
{
    leave_a_buffer();
    longjmp(gone, 1);
}

static int is_blocked(int signal)
{
    sigset_t set;
    sigprocmask(SIG_BLOCK, NULL, &set);
    return sigismember(&set, signal);
}

/* What SIGUSR2 being blocked looks like after a jump out of where it was. */
int blocked_after_jump(int save_mask)
{
    sigjmp_buf buffer;
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    if (sigsetjmp(buffer, save_mask)) {
        int blocked = is_blocked(SIGUSR2);
        sigprocmask(SIG_UNBLOCK, &set, NULL);
        return blocked;
    }
    sigprocmask(SIG_BLOCK, &set, NULL);
    siglongjmp(buffer, 1);
}
