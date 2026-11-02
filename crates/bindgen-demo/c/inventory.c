#include "inventory.h"

#include <stdlib.h>
#include <string.h>

struct inventory {
    struct item* items;
    size_t len;
};

inventory* inv_open(void) { return calloc(1, sizeof(inventory)); }

void inv_close(inventory* inv)
{
    free(inv->items);
    free(inv);
}

int inv_put(inventory* inv, const char* name, category cat, uint32_t qty, double price)
{
    if (strlen(name) >= INV_MAX_NAME)
        return -1;
    inv->items = realloc(inv->items, (inv->len + 1) * sizeof *inv->items);
    struct item* it = &inv->items[inv->len++];
    memset(it, 0, sizeof *it);
    strcpy(it->name, name);
    it->cat = cat;
    it->qty = qty;
    it->unit_price = price;
    return 0;
}

const struct item* inv_find(const inventory* inv, const char* name)
{
    for (size_t i = 0; i < inv->len; i++)
        if (!strcmp(inv->items[i].name, name))
            return &inv->items[i];
    return NULL;
}

size_t inv_each(const inventory* inv, item_fn f, void* ctx)
{
    size_t i = 0;
    for (; i < inv->len; i++)
        if (f(&inv->items[i], ctx))
            return i + 1;
    return i;
}

struct report inv_report(const inventory* inv)
{
    struct report r = { inv->len, 0, 0 };
    for (size_t i = 0; i < inv->len; i++) {
        r.units += inv->items[i].qty;
        r.value += inv->items[i].qty * inv->items[i].unit_price;
    }
    return r;
}
