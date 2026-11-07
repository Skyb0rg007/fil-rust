/* Imports nothing from libc, so it runs without one. It still lists libc.so as
   a dependency, as everything the compiler links does. */

#include <stdfil.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

int32_t add(int32_t a, int32_t b)
{
    return a + b;
}

const char* greeting(void)
{
    return "hello from Fil-C";
}

int32_t apply(int32_t (*function)(int32_t), int32_t value)
{
    return function(value);
}

char* pass_through(char* (*function)(char*), char* text)
{
    return function(text);
}

/* The closure's data, reached through both ways of asking for it. */

static int32_t closure_body(int32_t value)
{
    return value + *(int32_t*)zcallee_closure_data();
}

int32_t call_closure(int32_t* first, int32_t* second, int32_t value)
{
    int32_t (*closure)(int32_t) = zclosure_new(closure_body, first);
    int32_t before = closure(value);
    if (zclosure_get_data(closure) != first)
        return -1;
    zclosure_set_data(closure, second);
    return before * 1000 + closure(value);
}

struct node {
    struct node* next;
    int64_t value;
};

struct node* push(struct node* head, int64_t value)
{
    struct node* node = malloc(sizeof(struct node));
    node->next = head;
    node->value = value;
    return node;
}

int64_t sum(struct node* head)
{
    int64_t total = 0;
    for (; head; head = head->next)
        total += head->value;
    return total;
}

void release(struct node* head)
{
    while (head) {
        struct node* next = head->next;
        free(head);
        head = next;
    }
}

static _Thread_local int32_t per_thread;

int32_t add_to_thread(int32_t value)
{
    per_thread += value;
    return per_thread;
}

static int32_t constructed;

__attribute__((constructor)) static void construct(void)
{
    constructed = 42;
}

int32_t constructed_value(void)
{
    return constructed;
}

int32_t read_past_end(void)
{
    /* Sizes round up to the 16-byte word, so use one that's a whole word. */
    volatile int32_t* block = malloc(16);
    return block[4];
}
