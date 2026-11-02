// Implements api.h (bound by fil-rust-bindgen into tests/generated/api.rs).
#include "api.h"

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int api_call_count;
const char* api_last_name = "none";

struct registry {
    struct shape** shapes;
    size_t len, cap;
};

registry* registry_new(void)
{
    api_call_count++;
    return calloc(1, sizeof(registry));
}

void registry_free(registry* r)
{
    for (size_t i = 0; i < r->len; i++) {
        free((void*)r->shapes[i]->name);
        free(r->shapes[i]->points);
        free(r->shapes[i]);
    }
    free(r->shapes);
    free(r);
}

/* Copies the shape (deeply), so the caller's memory isn't retained. */
int registry_add(registry* r, const struct shape* s)
{
    api_call_count++;
    if (!s->name)
        return -1;
    if (r->len == r->cap) {
        r->cap = r->cap ? r->cap * 2 : 4;
        r->shapes = realloc(r->shapes, r->cap * sizeof *r->shapes);
    }
    struct shape* copy = malloc(sizeof *copy);
    *copy = *s;
    copy->name = strdup(s->name);
    copy->points = malloc(s->npoints * sizeof *s->points);
    memcpy(copy->points, s->points, s->npoints * sizeof *s->points);
    r->shapes[r->len++] = copy;
    api_last_name = copy->name;
    return (int)r->len - 1;
}

size_t registry_len(const registry* r) { return r->len; }

struct shape* registry_get(registry* r, size_t index)
{
    return index < r->len ? r->shapes[index] : NULL;
}

int registry_visit(registry* r, shape_visitor visit, void* user)
{
    int total = 0;
    for (size_t i = 0; i < r->len; i++)
        total += visit(r->shapes[i], user);
    return total;
}

int registry_visit_all(registry* r, const struct visitor_set* set)
{
    int n = 0;
    for (size_t i = 0; i < r->len; i++)
        if (set->on_shape(r->shapes[i], NULL))
            n++;
    if (set->on_done)
        set->on_done(n);
    return n;
}

struct bbox registry_bounds(const registry* r)
{
    struct bbox b = { 1e300, 1e300, -1e300, -1e300, 0 };
    for (size_t i = 0; i < r->len; i++) {
        const struct shape* s = r->shapes[i];
        for (size_t j = 0; j < s->npoints; j++) {
            double x = s->origin.x + s->points[j].x, y = s->origin.y + s->points[j].y;
            if (x < b.min_x) b.min_x = x;
            if (y < b.min_y) b.min_y = y;
            if (x > b.max_x) b.max_x = x;
            if (y > b.max_y) b.max_y = y;
            b.count++;
        }
    }
    return b;
}

struct vec2 vec2_add(struct vec2 a, struct vec2 b) { return (struct vec2){ a.x + b.x, a.y + b.y }; }

color color_mix(color a, color b)
{
    return (color){ (a.r + b.r) / 2, (a.g + b.g) / 2, (a.b + b.b) / 2, (a.a + b.a) / 2 };
}

double shape_area(const struct shape* s)
{
    double twice = 0;
    for (size_t i = 0; i < s->npoints; i++) {
        struct vec2 a = s->points[i], b = s->points[(i + 1) % s->npoints];
        twice += a.x * b.y - b.x * a.y;
    }
    return twice < 0 ? -twice / 2 : twice / 2;
}

struct tag_node* tags_split(const char* csv)
{
    struct tag_node* head = NULL;
    struct tag_node** tail = &head;
    while (*csv) {
        size_t n = strcspn(csv, ",");
        struct tag_node* node = malloc(sizeof *node);
        node->tag = strndup(csv, n);
        node->next = NULL;
        *tail = node;
        tail = &node->next;
        csv += n;
        if (*csv == ',')
            csv++;
    }
    return head;
}

int64_t sum_i64(const int64_t* values, size_t n)
{
    int64_t s = 0;
    for (size_t i = 0; i < n; i++)
        s += values[i];
    return s;
}

uint32_t hash_bytes(const void* data, size_t len)
{
    const unsigned char* p = data;
    uint32_t h = 2166136261u;
    for (size_t i = 0; i < len; i++)
        h = (h ^ p[i]) * 16777619u;
    return h;
}

bool is_kind(const struct shape* s, shape_kind k) { return s->kind == k; }

int api_printf(const char* fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    int n = vprintf(fmt, ap);
    va_end(ap);
    return n;
}
