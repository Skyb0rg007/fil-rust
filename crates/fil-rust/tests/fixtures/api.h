/* A header in the style of a real C library API, used to test
   fil-rust-bindgen. Implemented by api.c. */
#ifndef API_H
#define API_H

#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>

#define API_VERSION 3
#define API_NAME "api"
#define API_MAX_TAGS 4

typedef enum { SHAPE_CIRCLE, SHAPE_RECT = 5, SHAPE_TRIANGLE } shape_kind;

struct vec2 {
    double x, y;
};

typedef struct {
    uint8_t r, g, b, a;
} color;

/* A struct holding pointers, nested structs and an array. */
struct shape {
    shape_kind kind;
    const char* name;
    struct vec2 origin;
    struct vec2* points;
    size_t npoints;
    color fill;
    int16_t tags[API_MAX_TAGS];
    bool visible;
};

/* An opaque handle. */
typedef struct registry registry;

typedef int (*shape_visitor)(const struct shape* shape, void* user);

struct visitor_set {
    shape_visitor on_shape;
    void (*on_done)(int count);
};

/* A large struct passed and returned by value. */
struct bbox {
    double min_x, min_y, max_x, max_y;
    int32_t count;
};

/* A self-referential list. */
struct tag_node {
    const char* tag;
    struct tag_node* next;
};

extern int api_call_count;
extern const char* api_last_name;

registry* registry_new(void);
void registry_free(registry* r);
int registry_add(registry* r, const struct shape* s);
size_t registry_len(const registry* r);
struct shape* registry_get(registry* r, size_t index);
int registry_visit(registry* r, shape_visitor visit, void* user);
int registry_visit_all(registry* r, const struct visitor_set* set);
struct bbox registry_bounds(const registry* r);

struct vec2 vec2_add(struct vec2 a, struct vec2 b);
color color_mix(color a, color b);
double shape_area(const struct shape* s);
struct tag_node* tags_split(const char* csv);
int64_t sum_i64(const int64_t* values, size_t n);
uint32_t hash_bytes(const void* data, size_t len);
bool is_kind(const struct shape* s, shape_kind k);
int api_printf(const char* fmt, ...);

#endif
