/* The libc core, written out as a transcript: string, memory, ctype, number
 * parsing and allocation results, with errno where a call sets it. The same
 * source is built for the host, and the two transcripts must match. It's built
 * with -fno-builtin, so every call reaches the libc. */
#define _GNU_SOURCE
#include <ctype.h>
#include <malloc.h>
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>

static char out[1 << 16];
static size_t used;

static void text(const char *s) { while (*s && used < sizeof out) out[used++] = *s++; }
static void num(long long value)
{
    char digits[24];
    int n = 0;
    unsigned long long magnitude = value < 0 ? -(unsigned long long)value : (unsigned long long)value;
    if (value < 0) text("-");
    do digits[n++] = '0' + magnitude % 10; while (magnitude /= 10);
    while (n) { char c[2] = { digits[--n], 0 }; text(c); }
}
static void hex(unsigned long long value)
{
    char digits[17];
    int n = 0;
    do digits[n++] = "0123456789abcdef"[value % 16]; while (value /= 16);
    text("0x");
    while (n) { char c[2] = { digits[--n], 0 }; text(c); }
}
static void line(const char *name) { text("\n"); text(name); }
static void field(long long value) { text(" "); num(value); }
static void sign(int value) { field(value < 0 ? -1 : value > 0); }
static void at(const void *p, const void *base) { field(p ? (const char *)p - (const char *)base : -1); }
static void str(const char *s) { text(" \""); text(s); text("\""); }
static void bytes(const void *p, size_t n)
{
    text(" [");
    for (size_t i = 0; i < n; i++) { if (i) text(","); num(((const unsigned char *)p)[i]); }
    text("]");
}
static void error(void) { text(" errno="); num(errno); errno = 0; }

static void strings(void)
{
    static const char *words[] = { "", "a", "abc", "abd", "ABC", "ab", "abcdef", "\xff", "z" };
    const int count = sizeof words / sizeof *words;
    for (int i = 0; i < count; i++) {
        line("strlen"); field(strlen(words[i])); field(strnlen(words[i], 2));
        for (int j = 0; j < count; j++) {
            line("compare"); field(i); field(j);
            sign(strcmp(words[i], words[j])); sign(strncmp(words[i], words[j], 2));
            sign(strcasecmp(words[i], words[j])); sign(strncasecmp(words[i], words[j], 3));
            sign(strcoll(words[i], words[j]));
            sign(memcmp(words[i], words[j], 1)); field(bcmp(words[i], words[j], 1) != 0);
            at(strstr(words[i], words[j]), words[i]); at(strcasestr(words[i], words[j]), words[i]);
            at(memmem(words[i], strlen(words[i]), words[j], strlen(words[j])), words[i]);
            field(strspn(words[i], words[j])); field(strcspn(words[i], words[j]));
            at(strpbrk(words[i], words[j]), words[i]);
        }
    }

    const char *s = "hello, world";
    for (int c = 0; c < 128; c += 7) {
        line("search"); field(c);
        at(strchr(s, c), s); at(strrchr(s, c), s); at(strchrnul(s, c), s);
        at(memchr(s, c, 12), s); at(memrchr(s, c, 12), s); at(index(s, c), s); at(rindex(s, c), s);
    }
    line("search nul"); at(strchr(s, 0), s); at(strrchr(s, 0), s); at(rawmemchr(s, 'w'), s);

    char buffer[32];
    line("strcpy"); str(strcpy(buffer, "copy")); at(stpcpy(buffer, "stp"), buffer); str(buffer);
    memset(buffer, 'x', sizeof buffer);
    line("strncpy"); strncpy(buffer, "ab", 5); bytes(buffer, 7);
    memset(buffer, 'x', sizeof buffer);
    at(stpncpy(buffer, "abcdef", 3), buffer); bytes(buffer, 5);
    strcpy(buffer, "cat");
    line("strcat"); str(strcat(buffer, "dog")); str(strncat(buffer, "fish", 2));
    line("strlcpy"); field(strlcpy(buffer, "truncated", 5)); str(buffer);
    field(strlcat(buffer, "abcdef", 8)); str(buffer);
    line("memccpy"); memset(buffer, 0, sizeof buffer); at(memccpy(buffer, "key=value", '=', 9), buffer); str(buffer);
    at(memccpy(buffer, "abc", 'z', 3), buffer);
    line("mempcpy"); at(mempcpy(buffer, "12345", 5), buffer);
    line("memmove"); strcpy(buffer, "0123456789"); memmove(buffer + 2, buffer, 6); str(buffer);
    memmove(buffer, buffer + 3, 5); str(buffer);
    line("memset"); memset(buffer, 'q', 4); bzero(buffer + 1, 1); explicit_bzero(buffer + 2, 1); bytes(buffer, 5);

    char *copy = strdup("duplicate");
    line("strdup"); str(copy); free(copy);
    copy = strndup("duplicate", 3); str(copy); free(copy);
    copy = strndup("du", 30); str(copy); free(copy);

    char tokens[] = ",,one,two;;three,";
    line("strtok");
    for (char *token = strtok(tokens, ",;"); token; token = strtok(NULL, ",;")) { str(token); at(token, tokens); }
    char again[] = "a b  c";
    char *rest;
    line("strtok_r");
    for (char *token = strtok_r(again, " ", &rest); token; token = strtok_r(NULL, " ", &rest)) str(token);
    char fields[] = "x::y:";
    char *cursor = fields;
    line("strsep");
    for (char *token; (token = strsep(&cursor, ":"));) str(token);
    at(cursor, fields);
}

static void classes(void)
{
    for (int c = -1; c < 256; c++) {
        line("ctype"); field(c);
        field(!!isalnum(c)); field(!!isalpha(c)); field(!!isblank(c)); field(!!iscntrl(c));
        field(!!isdigit(c)); field(!!isgraph(c)); field(!!islower(c)); field(!!isprint(c));
        field(!!ispunct(c)); field(!!isspace(c)); field(!!isupper(c)); field(!!isxdigit(c));
        field(tolower(c)); field(toupper(c));
        /* The functions, not the macros' tables. */
        field(!!(isalpha)(c)); field(!!(isspace)(c)); field(!!(ispunct)(c)); field((tolower)(c)); field((toupper)(c));
        field(!!isascii(c)); field(toascii(c));
    }
}

static void numbers(void)
{
    static const char *integers[] = {
        "42", "  -17xyz", "0x1F", "0777", "z", "", "  +", "-0x", "9223372036854775807",
        "9223372036854775808", "-9223372036854775809", "18446744073709551616", "-1", "1e5", "0b101",
    };
    static const int bases[] = { 0, 10, 16, 8, 36, 2, 1, 37 };
    for (unsigned i = 0; i < sizeof integers / sizeof *integers; i++) {
        for (unsigned j = 0; j < sizeof bases / sizeof *bases; j++) {
            char *end;
            line("strtol"); str(integers[i]); field(bases[j]);
            errno = 0;
            /* An invalid base leaves end alone. */
            end = NULL; field(strtol(integers[i], &end, bases[j])); at(end, integers[i]); error();
            end = NULL; field((long long)strtoul(integers[i], &end, bases[j])); at(end, integers[i]); error();
            end = NULL; field(strtoll(integers[i], &end, bases[j])); at(end, integers[i]); error();
            end = NULL; field((long long)strtoull(integers[i], &end, bases[j])); at(end, integers[i]); error();
        }
        line("atoi"); str(integers[i]); field(atoi(integers[i])); field(atol(integers[i])); field(atoll(integers[i]));
    }
    static const char *reals[] = {
        "3.25abc", "  -0.5", "1e400", "1e-400", "nan", "-inf", "infinity", "0x1p3", ".", "", "1e", "4.9e-324",
    };
    for (unsigned i = 0; i < sizeof reals / sizeof *reals; i++) {
        char *end;
        union { double d; uint64_t bits; } d;
        union { float f; uint32_t bits; } f;
        line("strtod"); str(reals[i]);
        errno = 0;
        d.d = strtod(reals[i], &end); hex(d.bits); at(end, reals[i]); error();
        f.f = strtof(reals[i], &end); text(" "); hex(f.bits); at(end, reals[i]); error();
        d.d = atof(reals[i]); text(" "); hex(d.bits);
    }
    line("strtol null end"); field(strtol("12", NULL, 10));
}

static void allocation(void)
{
    errno = 0;
    line("calloc");
    unsigned char *zeroed = calloc(5, 3);
    field(zeroed != NULL); bytes(zeroed, 15); free(zeroed);
    field(calloc(SIZE_MAX / 2, 3) == NULL); error();
    line("realloc");
    char *grown = realloc(NULL, 4);
    strcpy(grown, "abc");
    grown = realloc(grown, 100);
    str(grown);
    grown = reallocarray(grown, 2, 10);
    str(grown);
    field(reallocarray(grown, SIZE_MAX / 2, 3) == NULL); error();
    free(grown);
    line("aligned");
    void *aligned = aligned_alloc(64, 64);
    field((uintptr_t)aligned % 64); free(aligned);
    void *place = NULL;
    field(posix_memalign(&place, 256, 10)); field((uintptr_t)place % 256); free(place);
    field(posix_memalign(&place, 3, 10));
    field(posix_memalign(&place, 4, 10));
    aligned = memalign(128, 1);
    field((uintptr_t)aligned % 128); free(aligned);
    line("malloc"); free(NULL); field(malloc_usable_size(NULL));
}

size_t transcript_length(void)
{
    if (!used) {
        strings();
        classes();
        numbers();
        allocation();
    }
    return used;
}

int transcript_byte(size_t index) { return (unsigned char)out[index]; }

/* Misuse, each run in a process of its own. */
size_t length_of_unterminated(void)
{
    char *bytes = malloc(4);
    memset(bytes, 'x', malloc_usable_size(bytes));
    return strlen(bytes);
}

void copy_past_the_end(void)
{
    char *small = malloc(4);
    strcpy(small, "longer than any small object");
}

void move_past_the_end(void)
{
    static char big[100];
    char *small = malloc(4);
    memmove(small, big, sizeof big);
}

/* Reading on past the digits would trap. */
long parse_unterminated(void)
{
    char *digits = malloc(3);
    memset(digits, '7', malloc_usable_size(digits));
    return strtol(digits, NULL, 10);
}

/* Not misuse: the number ends before the buffer does. */
long parse_in_a_buffer(void)
{
    char *buffer = malloc(4);
    memcpy(buffer, "12,3", 4);
    char *end;
    long value = strtol(buffer, &end, 10);
    return value * 10 + (end - buffer);
}

/* What the race test hammers: a buffer whose terminator another thread keeps
 * moving. */
static char moving[64];

void move_terminators(int rounds)
{
    for (int round = 0; round < rounds; round++) {
        for (int i = 0; i < 63; i++) moving[i] = '1';
        moving[63] = 0;
        for (int i = 62; i >= 0; i--) moving[i] = 0;
    }
}

long read_moving(int rounds)
{
    long total = 0;
    char copy[64];
    for (int round = 0; round < rounds; round++) {
        size_t length = strlen(moving);
        if (length > 63) return -1;
        total += length;
        total += strtol(moving, NULL, 10) > 0;
        total += strchr(moving, 0) != NULL;
        strncpy(copy, moving, sizeof copy);
        total += strnlen(copy, sizeof copy) <= 63;
        free(strdup(moving));
    }
    return total;
}
