#include <stdfil.h>

long realloc_keeps_contents(void) {
    long** p = zgc_alloc(3 * sizeof(long*));
    long value = 7;
    p[2] = &value;
    p = zgc_realloc(p, 100 * sizeof(long*));
    long result = *p[2];
    zgc_free(p);
    return result;
}

long aligned_allocation(void) {
    char* p = zgc_aligned_alloc(4096, 100);
    long aligned = ((unsigned long)p & 4095) == 0;
    p[99] = 1;
    zgc_free(p);
    return aligned;
}

long capabilities(void) {
    char* p = zgc_alloc(32);
    long result = 0;
    result += (char*)zgetlower(p + 3) == p;
    result += (char*)zgetupper(p + 3) == p + 32;
    result += zhasvalidcap(p);
    zgc_free(p);
    result += !zhasvalidcap(p);
    return result;
}

long string_length(void) { return zstrlen("hello"); }

/* These come from the support library, compiled with Fil-C. */
long collection_request(void) { zgc_request_and_wait(); return 1; }
long formatting(void) { char* s = zasprintf("%d-%s", 42, "x"); return zstrlen(s); }

void user_error(void) { zerror("gave up"); }
void unterminated(void) {
    char* s = zgc_alloc(16);
    for (int i = 0; i < 16; i++) s[i] = 'x';
    zstrlen(s);
}
