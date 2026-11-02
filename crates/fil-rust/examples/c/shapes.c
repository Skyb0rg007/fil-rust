#include <math.h>
#include <stdlib.h>
#include <string.h>

struct point {
    double x, y;
};

struct polygon {
    const char* name;
    struct point* points;
    size_t count;
};

struct node {
    int value;
    struct node* next;
};

struct point midpoint(struct point a, struct point b)
{
    return (struct point){ (a.x + b.x) / 2, (a.y + b.y) / 2 };
}

/* Shoelace formula. */
double polygon_area(const struct polygon* p)
{
    double twice = 0;
    for (size_t i = 0; i < p->count; i++) {
        struct point a = p->points[i], b = p->points[(i + 1) % p->count];
        twice += a.x * b.y - b.x * a.y;
    }
    return fabs(twice) / 2;
}

void polygon_scale(struct polygon* p, double k)
{
    for (size_t i = 0; i < p->count; i++) {
        p->points[i].x *= k;
        p->points[i].y *= k;
    }
}

/* A polygon built entirely by C: a regular n-gon. */
struct polygon regular(const char* name, size_t n, double radius)
{
    struct polygon p = { strdup(name), calloc(n, sizeof(struct point)), n };
    for (size_t i = 0; i < n; i++) {
        double t = 2 * M_PI * i / n;
        p.points[i] = (struct point){ radius * cos(t), radius * sin(t) };
    }
    return p;
}

/* A list of the first n squares, built by C. */
struct node* squares(int n)
{
    struct node* head = NULL;
    for (int i = n; i >= 1; i--) {
        struct node* node = malloc(sizeof *node);
        node->value = i * i;
        node->next = head;
        head = node;
    }
    return head;
}
