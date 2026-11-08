/* The parts of stdfil.h that ask the runtime about itself. Each function
   returns 0, or the line that went wrong. */

#include <errno.h>
#include <filc_test_support.h>
#include <pizlonated_runtime.h>
#include <stdbool.h>
#include <stdfil.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CHECK(condition) do { if (!(condition)) return __LINE__; } while (0)

int settings(void)
{
    zset_errno(EDOM);
    CHECK(errno == EDOM);
    CHECK(!zget_quiet_panic());
    zset_quiet_panic(true);
    CHECK(zget_quiet_panic());
    zset_quiet_panic(false);
    CHECK(!zget_quiet_panic());
    CHECK(zgc_page_size() == (size_t)sysconf(_SC_PAGESIZE));
    zscavenger_suspend();
    zscavenger_resume();
    zscavenge_synchronously();
    return 0;
}

int describing_pointers(void)
{
    char* bytes = malloc(16);
    char expected[128];
    snprintf(expected, sizeof(expected), "%p,%p,%p", bytes + 1, bytes, bytes + 16);
    CHECK(!strcmp(zptr_to_new_string(bytes + 1), expected));

    struct { char* pointer; long number; }* pair = malloc(sizeof(*pair));
    pair->pointer = bytes;
    pair->number = 5;
    snprintf(expected, sizeof(expected), ":[+0:%p,%p,%p +8:0x5]", bytes, bytes, bytes + 16);
    char* contents = zptr_contents_to_new_string(pair);
    CHECK(strlen(contents) > strlen(expected));
    CHECK(!strcmp(contents + strlen(contents) - strlen(expected), expected));

    zvalidate_ptr(bytes);
    zvalidate_ptr(pair);
    zvalidate_ptr(NULL);
    CHECK(zgc_get_allocation_size(bytes) >= 16);
    return 0;
}

/* An address turned into a pointer has no capability until it's given one. */
int setting_capabilities(void)
{
    char* target = malloc(8);
    strcpy(target, "target");
    /* Through memory as an integer, so the compiler can't carry the
       capability along. */
    volatile uintptr_t address = (uintptr_t)target;
    char** slots = malloc(2 * sizeof(char*));
    slots[0] = (char*)address;
    slots[1] = (char*)(address + 1);
    CHECK(!zhasvalidcap(slots[0]) && !zhasvalidcap(slots[1]));
    zsetcap(slots, target, 2 * sizeof(char*));
    CHECK(zhasvalidcap(slots[0]) && slots[0] == target && !strcmp(slots[0], "target"));
    CHECK(zhasvalidcap(slots[1]) && !strcmp(slots[1], "arget"));
    return 0;
}

/* A request doesn't wait, but the collection it asks for happens. */
int requesting_collections(void)
{
    zgc_request_and_wait();
    zgc_cycle_number completed = zgc_completed_cycle();
    CHECK(zgc_requested_cycle() >= completed);
    zgc_cycle_number requested = zgc_try_request();
    CHECK(requested > completed);
    CHECK(zgc_requested_cycle() >= requested);
    volatile unsigned long spins = 0;
    while (zgc_completed_cycle() < requested && spins < 400000000ul)
        spins++;
    CHECK(zgc_completed_cycle() >= requested);
    return 0;
}

int printing(void)
{
    zprint_long(12345);
    zprint("\n");
    zprint_ptr(malloc(16));
    zprint("\n");
    zdump_stack();
    zdump_stacks();
    return 0;
}
