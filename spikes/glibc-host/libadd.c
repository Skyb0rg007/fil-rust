#include <string.h>
#include <stdlib.h>
int add(int a, int b) { return a + b; }
size_t my_strlen(const char *s) { return strlen(s); }
char *dup_upper(const char *s) { char *r = strdup(s); for (char *p = r; *p; p++) if (*p >= 'a' && *p <= 'z') *p -= 32; return r; }
struct pt { int x, y; };
struct pt mkpt(int x, int y) { struct pt p = {x, y}; return p; }
int global_counter;
