#include <stdlib.h>

int* make_array(int n)
{
    int* a = malloc(n * sizeof(int));
    for (int i = 0; i < n; i++)
        a[i] = i * i;
    return a;
}

void release(void* p) { free(p); }

/* No bounds check: Fil-C catches the bug instead. */
int nth(const int* a, int i) { return a[i]; }
