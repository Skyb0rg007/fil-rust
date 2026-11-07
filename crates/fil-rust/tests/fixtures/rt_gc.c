/* One function for each way compiled code can hold memory the collector has to
   find, or give back. None of it imports from libc. `collect` is a Rust
   callback that runs a collection, so these are collections with compiled
   frames on the stack. */

#include <stdarg.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdfil.h>
#include <stdint.h>
#include <stdlib.h>

typedef void (*collect_fn)(void);

struct node {
    struct node* next;
    int64_t value;
};

static struct node* make(int64_t value, struct node* next)
{
    struct node* node = malloc(sizeof(struct node));
    node->next = next;
    node->value = value;
    return node;
}

/* Stored here, a pointer to a local makes the local escape, which puts it in
   the heap. Clearing this right after lets it go. */
static void* volatile escape;

__attribute__((noinline)) static void opaque(void* pointer)
{
    escape = pointer;
    escape = NULL;
}

/* Only a frame slot refers to the node. */
int64_t frame_slot(collect_fn collect, int64_t value)
{
    struct node* volatile local = make(value, NULL);
    collect();
    return local->value;
}

/* Pointers in a local array, which stays on the stack with an explicit aux.
   One loop keeps them from also being in a frame slot, as each overwrites the
   last, so the aux is the only thing that refers to all of them. */
int64_t stack_aux(collect_fn collect, int64_t value, int count)
{
    struct node* locals[8];
    for (int index = 0; index < 8; index++)
        locals[index] = NULL;
    for (int index = 0; index < count; index++)
        locals[index] = make(value + index, NULL);
    collect();
    int64_t total = 0;
    for (int index = 0; index < count; index++)
        total += locals[index]->value;
    return total;
}

/* A local whose address gets out, so it's a heap block. */
int64_t escaping_local(collect_fn collect, int64_t value)
{
    struct node local = { make(value, NULL), 0 };
    opaque(&local);
    collect();
    return local.next->value;
}

struct pair {
    struct node* first;
    struct node* second;
    int64_t padding[4];
};

__attribute__((noinline)) int64_t by_value_callee(struct pair pair, collect_fn collect)
{
    collect();
    return pair.first->value + pair.second->value;
}

int64_t by_value(collect_fn collect, int64_t value)
{
    struct pair pair = { make(value, NULL), make(1, NULL), { 0 } };
    return by_value_callee(pair, collect);
}

__attribute__((noinline)) static int64_t variadic_callee(collect_fn collect, ...)
{
    va_list list;
    va_start(list, collect);
    collect();
    struct node* node = va_arg(list, struct node*);
    va_end(list);
    return node->value;
}

int64_t variadic(collect_fn collect, int64_t value)
{
    return variadic_callee(collect, make(value, NULL));
}

/* Heap objects that refer to each other, reached through an aux array. */
int64_t chain(collect_fn collect, int64_t length)
{
    struct node* volatile head = NULL;
    for (int64_t index = 0; index < length; index++)
        head = make(index, head);
    collect();
    int64_t total = 0;
    for (struct node* node = head; node; node = node->next)
        total += node->value;
    return total;
}

/* A pointer in an atomic box. */
int64_t atomic_box(collect_fn collect, int64_t value)
{
    _Atomic(struct node*)* volatile cell = malloc(sizeof(*cell));
    struct node* expected = NULL;
    atomic_compare_exchange_strong(cell, &expected, make(value, NULL));
    collect();
    return atomic_load(cell)->value;
}

static int64_t closure_body(void)
{
    return ((struct node*)zcallee_closure_data())->value;
}

int64_t closure_data(collect_fn collect, int64_t value)
{
    int64_t (*volatile closure)(void) = zclosure_new(closure_body, make(value, NULL));
    collect();
    return closure();
}

static _Thread_local struct node* volatile per_thread;

__attribute__((noinline)) static void set_thread_local(int64_t value)
{
    per_thread = make(value, NULL);
}

int64_t thread_local(collect_fn collect, int64_t value)
{
    set_thread_local(value);
    collect();
    return per_thread->value;
}

/* The libc keeps its thread structure here. */
void* zthread_self_cookie(void);
void zthread_set_self_cookie(void* cookie);

__attribute__((noinline)) static void set_cookie(int64_t value)
{
    zthread_set_self_cookie(make(value, NULL));
}

int64_t thread_cookie(collect_fn collect, int64_t value)
{
    set_cookie(value);
    collect();
    return ((struct node*)zthread_self_cookie())->value;
}

/* Threads, which run with no libc here, as nothing stops them. */
void* zthread_create(void* (*callback)(void* arg), void* arg);
_Bool zthread_join(void* thread, void** result);
unsigned zthread_get_id(void* thread);
void zthread_exit(void* result);

static void* return_node(void* value)
{
    return make((int64_t)value, NULL);
}

__attribute__((noinline)) static void exit_deep_down(int64_t depth, int64_t value)
{
    if (depth)
        exit_deep_down(depth - 1, value);
    zthread_exit(make(value, NULL));
}

static void* exit_with_node(void* value)
{
    exit_deep_down(16, (int64_t)value);
    return NULL;
}

static void* do_nothing(void* unused)
{
    return unused;
}

/* All a finished thread is good for is what it returned. */
int64_t thread_result(collect_fn collect, int64_t value, int by_exiting)
{
    void* thread = zthread_create(by_exiting ? exit_with_node : return_node, (void*)value);
    zthread_join(thread, NULL);
    collect();
    void* result = NULL;
    zthread_join(thread, &result);
    return ((struct node*)result)->value;
}

/* A capability to a thread that's gone is still a capability. If the memory
   behind it had been reused, this would be some other thread. */
_Bool finished_thread_keeps_its_identity(collect_fn collect)
{
    void* thread = zthread_create(do_nothing, NULL);
    zthread_join(thread, NULL);
    unsigned id = zthread_get_id(thread);
    collect();
    void* other = zthread_create(do_nothing, NULL);
    zthread_join(other, NULL);
    return zthread_get_id(thread) == id;
}

void churn_threads(int64_t count)
{
    for (int64_t index = 0; index < count; ++index)
        zthread_join(zthread_create(do_nothing, NULL), NULL);
}

/* A global, and what it refers to. */
static struct node* volatile global;

void set_global(int64_t value)
{
    global = make(value, NULL);
}

void clear_global(void)
{
    global = NULL;
}

int64_t read_global(void)
{
    return global->value;
}

/* Memory `free` gave up, with references left behind. */
/* The plain field's aux entry is a lower. Being volatile, the other is atomic,
   and its entry is a box. */
struct holder {
    struct node* freed;
    struct node* volatile freed_boxed;
    struct node* kept;
};

struct holder* make_holder(int64_t value, size_t padding)
{
    struct holder* holder = malloc(sizeof(struct holder));
    holder->freed = malloc(sizeof(struct node) + padding);
    holder->freed->value = value;
    holder->freed_boxed = holder->freed;
    holder->kept = make(value, NULL);
    free(holder->freed);
    return holder;
}

int64_t read_kept(struct holder* holder)
{
    return holder->kept->value;
}

struct node** freed_field(struct holder* holder, int boxed)
{
    return boxed ? (struct node**)&holder->freed_boxed : &holder->freed;
}

/* Garbage of each kind, to be left for the collector. */

/* Once per call: a loop in here would only reuse the one block. */
__attribute__((noinline)) static int64_t one_local(int64_t value)
{
    struct node local = { NULL, value };
    opaque(&local);
    return local.value;
}

int64_t churn_locals(int64_t rounds)
{
    int64_t total = 0;
    for (int64_t round = 0; round < rounds; round++)
        total += one_local(round);
    return total;
}

__attribute__((noinline)) static int64_t churn_callee(struct pair pair, ...)
{
    va_list list;
    va_start(list, pair);
    (void)va_arg(list, int64_t);
    struct node* node = va_arg(list, struct node*);
    va_end(list);
    return node->value + pair.padding[0];
}

int64_t churn_calls(int64_t rounds)
{
    int64_t total = 0;
    for (int64_t round = 0; round < rounds; round++) {
        struct pair pair = { NULL, NULL, { round } };
        total += churn_callee(pair, round, make(round, NULL));
    }
    return total;
}

int64_t churn_malloc(int64_t rounds, size_t size)
{
    int64_t total = 0;
    for (int64_t round = 0; round < rounds; round++) {
        char* volatile block = malloc(size);
        block[0] = 1;
        total += block[0];
    }
    return total;
}

void churn_freed(int64_t rounds)
{
    for (int64_t round = 0; round < rounds; round++)
        free(make(round, NULL));
}

int64_t aligned(collect_fn collect, int64_t value)
{
    int64_t* volatile* volatile block = zgc_aligned_alloc(4096, sizeof(void*) * 2);
    block[1] = (int64_t*)make(value, NULL);
    collect();
    return ((struct node*)block[1])->value;
}

/* Allocates with a node in its frame, polling as loops do, until the flag says
   to stop. */
_Atomic int64_t rounds;

int64_t allocate_until(volatile int32_t* stop, int64_t value)
{
    struct node* volatile kept = make(value, NULL);
    while (!*stop) {
        char* volatile block = malloc(1024);
        block[0] = 1;
        rounds++;
    }
    return kept->value;
}
