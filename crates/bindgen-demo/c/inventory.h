#ifndef INVENTORY_H
#define INVENTORY_H

#include <stddef.h>
#include <stdint.h>

#define INV_MAX_NAME 32

typedef enum { CAT_TOOL = 1, CAT_FOOD, CAT_PART } category;

struct item {
    char name[INV_MAX_NAME];
    category cat;
    uint32_t qty;
    double unit_price;
};

typedef struct inventory inventory;

/* Called for each item; returns nonzero to stop early. */
typedef int (*item_fn)(const struct item* it, void* ctx);

struct report {
    size_t items;
    uint64_t units;
    double value;
};

inventory* inv_open(void);
void inv_close(inventory* inv);
int inv_put(inventory* inv, const char* name, category cat, uint32_t qty, double price);
const struct item* inv_find(const inventory* inv, const char* name);
size_t inv_each(const inventory* inv, item_fn f, void* ctx);
struct report inv_report(const inventory* inv);

#endif
