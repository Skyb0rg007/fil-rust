/* Pointer tables, weak references, weak maps and finalizer queues. Each
   function returns 0, or the line that went wrong.

   What should become unreachable is made in a function of its own that isn't
   inlined: once it returns, nothing refers to it but what's being tested. */

#include <stdfil.h>
#include <stdlib.h>
#include <string.h>

#define CHECK(condition) do { if (!(condition)) return __LINE__; } while (0)
#define ALONE __attribute__((noinline))

static void collect(void)
{
    zgc_request_and_wait();
}

static ALONE char* text(const char* contents)
{
    char* result = malloc(32);
    strcpy(result, contents);
    return result;
}

static ALONE size_t encode_new(zptrtable* table, const char* contents)
{
    return zptrtable_encode(table, text(contents));
}

int ptr_tables(void)
{
    zptrtable* table = zptrtable_new();
    CHECK(zptrtable_encode(table, NULL) == 0);
    char* a = text("alpha");
    char* b = text("beta");
    size_t encoded_a = zptrtable_encode(table, a);
    size_t encoded_b = zptrtable_encode(table, b);
    CHECK(encoded_a && encoded_b && encoded_a != encoded_b);
    CHECK(zptrtable_encode(table, a) == encoded_a);
    size_t encoded_inside = zptrtable_encode(table, a + 1);
    CHECK(encoded_inside != encoded_a && encoded_inside != encoded_b);
    CHECK(zptrtable_decode(table, encoded_a) == a);
    CHECK(zptrtable_decode(table, encoded_inside) == a + 1);
    CHECK(zptrtable_decode(table, 12345) == NULL);
    CHECK(zptrtable_decode(table, encoded_b + (1000 << 4)) == NULL);
    free(b);
    CHECK(zptrtable_decode(table, encoded_b) == NULL);
    collect();
    /* The freed one's index goes to the next pointer. */
    char* c = text("gamma");
    CHECK(zptrtable_encode(table, c) == encoded_b);
    CHECK(zptrtable_decode(table, encoded_b) == c);
    return 0;
}

int ptr_tables_keep_what_they_name(void)
{
    zptrtable* table = zptrtable_new();
    size_t encoded = encode_new(table, "kept by the table");
    collect();
    collect();
    char* kept = zptrtable_decode(table, encoded);
    CHECK(kept && !strcmp(kept, "kept by the table"));
    return 0;
}

static ALONE size_t exact_new(zexact_ptrtable* table, const char* contents)
{
    return zexact_ptrtable_encode(table, text(contents));
}

int exact_tables(void)
{
    zexact_ptrtable* strong = zexact_ptrtable_new();
    zexact_ptrtable* weak = zexact_ptrtable_new_weak();
    CHECK(!zexact_ptrtable_is_weak(strong));
    CHECK(zexact_ptrtable_is_weak(weak));
    char* a = text("a");
    CHECK(zexact_ptrtable_encode(strong, a) == (size_t)a);
    CHECK(zexact_ptrtable_decode(strong, (size_t)a) == a);
    CHECK(zhasvalidcap(zexact_ptrtable_decode(strong, (size_t)a)));
    CHECK(!zhasvalidcap(zexact_ptrtable_decode(strong, 4096)));
    CHECK(!zhasvalidcap(zexact_ptrtable_decode(weak, (size_t)a)));
    size_t address = (size_t)a;
    free(a);
    CHECK(!zhasvalidcap(zexact_ptrtable_decode(strong, address)));
    size_t in_strong = exact_new(strong, "strong");
    size_t in_weak = exact_new(weak, "weak");
    collect();
    char* kept = zexact_ptrtable_decode(strong, in_strong);
    CHECK(zhasvalidcap(kept) && !strcmp(kept, "strong"));
    CHECK(!zhasvalidcap(zexact_ptrtable_decode(weak, in_weak)));
    /* A weak table hands back what's still alive. */
    char* alive = text("alive");
    zexact_ptrtable_encode(weak, alive);
    collect();
    CHECK(zexact_ptrtable_decode(weak, (size_t)alive) == alive);
    return 0;
}

static ALONE zweak* weak_to_new(const char* contents)
{
    return zweak_new(text(contents));
}

int weak_references(void)
{
    char* kept = text("kept");
    zweak* to_kept = zweak_new(kept);
    zweak* to_lost = weak_to_new("lost");
    zweak* to_null = zweak_new(NULL);
    collect();
    CHECK(zweak_get(to_kept) == kept);
    CHECK(zweak_get(to_lost) == NULL);
    CHECK(zweak_get(to_null) == NULL);
    CHECK(!strcmp(kept, "kept"));
    return 0;
}

static ALONE zweak* add_chain(zweak_map* map, char* root)
{
    /* root -> a -> b, with a and b only reachable through the map. */
    char* a = text("a");
    char* b = text("b");
    zweak_map_set(map, root, a);
    zweak_map_set(map, a, b);
    return zweak_new(b);
}

static ALONE zweak* add_lost(zweak_map* map)
{
    char* key = text("key");
    zweak_map_set(map, key, text("value"));
    /* A value that refers to its key doesn't keep it alive. */
    char* own = text("own");
    zweak_map_set(map, own, own);
    return zweak_new(own);
}

int weak_maps(void)
{
    zweak_map* map = zweak_map_new();
    char* root = text("root");
    zweak* to_b = add_chain(map, root);
    zweak* to_own = add_lost(map);
    CHECK(zweak_map_size(map) == 4);
    collect();
    CHECK(zweak_map_size(map) == 2);
    CHECK(zweak_get(to_own) == NULL);
    char* a = zweak_map_get(map, root);
    CHECK(a && !strcmp(a, "a"));
    char* b = zweak_map_get(map, a);
    CHECK(b && !strcmp(b, "b") && zweak_get(to_b) == b);
    CHECK(zweak_map_get(map, root + 1) == NULL);
    size_t count = 0;
    zweak_map_iter* iter = zweak_map_get_iter(map);
    while (zweak_map_iter_next(iter)) {
        void* key = zweak_map_iter_key(iter);
        void* value = zweak_map_iter_value(iter);
        CHECK((key == root && value == a) || (key == a && value == b));
        count++;
    }
    CHECK(count == 2);
    zweak_map_set(map, a, NULL);
    CHECK(zweak_map_size(map) == 1 && zweak_map_get(map, a) == NULL);
    free(root);
    CHECK(zweak_map_get(map, root) == NULL);
    return 0;
}

static ALONE zweak* finalizable(zgc_finq* queue, const char* contents)
{
    char* object = zgc_finq_alloc(queue, 32);
    strcpy(object, contents);
    return zweak_new(object);
}

int finalizers(void)
{
    zgc_finq* queue = zgc_finq_new();
    CHECK(zgc_finq_poll(queue) == NULL);
    char* kept = zgc_finq_alloc(queue, 16);
    strcpy(kept, "kept");
    zweak* weak = finalizable(queue, "finalize me");
    collect();
    /* Weak references are gone before the finalizer gets it. */
    CHECK(zweak_get(weak) == NULL);
    char* object = zgc_finq_poll(queue);
    CHECK(object && !strcmp(object, "finalize me"));
    CHECK(zgc_finq_poll(queue) == NULL);
    /* Once finalized, it goes like anything else. */
    object = NULL;
    collect();
    CHECK(zgc_finq_poll(queue) == NULL);
    finalizable(queue, "waited for");
    collect();
    object = zgc_finq_wait(queue);
    CHECK(object && !strcmp(object, "waited for"));
    CHECK(!strcmp(kept, "kept"));
    return 0;
}

static ALONE void lose_queue(void)
{
    zgc_finq* queue = zgc_finq_new();
    finalizable(queue, "nobody will see this");
}

/* With its queue gone, there's no one to finalize an object for. */
int finalizers_without_a_queue(void)
{
    lose_queue();
    collect();
    collect();
    return 0;
}

struct holder {
    zweak_map* map;
    zweak* weak;
};

static ALONE void hold_lost_things(zgc_finq* queue)
{
    struct holder* holder = zgc_finq_alloc(queue, sizeof(struct holder));
    holder->map = zweak_map_new();
    zweak_map_set(holder->map, text("key"), text("value"));
    holder->weak = zweak_new(text("lost"));
}

/* Weak maps and weak references that only a finalizable object reaches still
   let go of what died: they mustn't hand back something that was collected. */
int weak_things_reached_by_finalizable_objects(void)
{
    zgc_finq* queue = zgc_finq_new();
    hold_lost_things(queue);
    collect();
    struct holder* holder = zgc_finq_poll(queue);
    CHECK(holder);
    zweak_map_iter* iter = zweak_map_get_iter(holder->map);
    CHECK(!zweak_map_iter_next(iter));
    CHECK(zweak_get(holder->weak) == NULL);
    return 0;
}
