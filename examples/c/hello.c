#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int add(int a, int b) { return a + b; }

/* Returns a new string; the caller frees it (or lets the GC have it). */
char* greet(const char* name)
{
    size_t n = strlen(name) + sizeof("Hello, !");
    char* out = malloc(n);
    snprintf(out, n, "Hello, %s!", name);
    return out;
}

/* Upper-cases a buffer in place. */
void shout(char* s)
{
    for (; *s; s++)
        *s = toupper((unsigned char)*s);
}

double average(const double* values, size_t n)
{
    double sum = 0;
    for (size_t i = 0; i < n; i++)
        sum += values[i];
    return n ? sum / n : 0;
}
