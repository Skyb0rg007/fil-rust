/* The libc core, written out as a transcript: string, memory, ctype, number
 * parsing and allocation results, with errno where a call sets it. The same
 * source is built for the host, and the two transcripts must match. It's built
 * with -fno-builtin, so every call reaches the libc. */
#define _GNU_SOURCE
#include <ctype.h>
#include <langinfo.h>
#include <limits.h>
#include <locale.h>
#include <stdio.h>
#include <wchar.h>
#include <wctype.h>
#include <malloc.h>
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <sys/stat.h>
#include <sys/utsname.h>
#include <time.h>
#include <unistd.h>

extern char **environ;
extern int __xpg_strerror_r(int, char *, size_t);

static char out[1 << 18];
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

static void errors(void)
{
    for (int number = -1; number < 140; number++) {
        line("strerror"); field(number); str(strerror(number));
        const char *description = strerrordesc_np(number);
        str(description ? description : "(none)");
    }
    char small[8];
    line("__xpg_strerror_r");
    field(__xpg_strerror_r(ENOENT, small, sizeof small)); str(small);
    char big[128];
    field(__xpg_strerror_r(ENOENT, big, sizeof big)); str(big);
    field(__xpg_strerror_r(100000, big, sizeof big));
    line("strerror_r"); str(strerror_r(EINVAL, big, sizeof big));
    for (int signal = 0; signal < 66; signal += 5) {
        line("strsignal"); field(signal); str(strsignal(signal));
        const char *description = sigdescr_np(signal);
        str(description ? description : "(none)");
    }
}

static int in_environ(const char *entry)
{
    for (char **e = environ; *e; e++)
        if (!strcmp(*e, entry)) return 1;
    return 0;
}

static void environment(void)
{
    const char *name = "FIL_RUST_LIBC_CORE_VARIABLE";
    errno = 0;
    line("getenv"); field(getenv(name) == NULL); field(getenv("") == NULL); field(getenv("A=B") == NULL);
    line("setenv"); field(setenv(name, "one", 0)); str(getenv(name)); field(in_environ("FIL_RUST_LIBC_CORE_VARIABLE=one"));
    field(setenv(name, "two", 0)); str(getenv(name));
    field(setenv(name, "two", 1)); str(getenv(name)); field(in_environ("FIL_RUST_LIBC_CORE_VARIABLE=two"));
    field(setenv("", "x", 1)); error(); field(setenv("A=B", "x", 1)); error();
    line("putenv"); static char entry[] = "FIL_RUST_LIBC_CORE_VARIABLE=three"; field(putenv(entry)); str(getenv(name));
    line("unsetenv"); field(unsetenv(name)); field(getenv(name) == NULL); field(in_environ("FIL_RUST_LIBC_CORE_VARIABLE=three"));
    field(unsetenv("A=B")); error();
}

static void tm_fields(const struct tm *tm)
{
    field(tm->tm_sec); field(tm->tm_min); field(tm->tm_hour); field(tm->tm_mday); field(tm->tm_mon);
    field(tm->tm_year); field(tm->tm_wday); field(tm->tm_yday); field(tm->tm_isdst); field(tm->tm_gmtoff);
    str(tm->tm_zone ? tm->tm_zone : "(null)");
}

static void times(void)
{
    static const time_t moments[] = { 0, 86400 * 365, 951782400, 1700000000, -1, 4102444800 };
    for (unsigned i = 0; i < sizeof moments / sizeof *moments; i++) {
        struct tm tm;
        char buffer[128];
        line("gmtime_r"); field(moments[i]); field(gmtime_r(&moments[i], &tm) == &tm); tm_fields(&tm);
        line("timegm"); field(timegm(&tm));
        line("localtime_r"); field(localtime_r(&moments[i], &tm) == &tm); tm_fields(&tm);
        line("mktime"); field(mktime(&tm)); tm_fields(&tm);
        line("strftime"); field(strftime(buffer, sizeof buffer, "%Y-%m-%d %H:%M:%S %Z %z %a %b %j %%", &tm)); str(buffer);
        field(strftime(buffer, 4, "%Y-%m-%d", &tm));
        line("asctime_r"); str(asctime_r(&tm, buffer));
        line("ctime_r"); str(ctime_r(&moments[i], buffer));
        line("localtime"); tm_fields(localtime(&moments[i])); tm_fields(gmtime(&moments[i]));
    }
    struct tm odd = { .tm_year = 124, .tm_mon = 13, .tm_mday = 40, .tm_hour = 25, .tm_isdst = -1 };
    line("mktime normalizes"); field(mktime(&odd)); tm_fields(&odd);
    line("difftime"); field((long long)difftime(100, 40));
    tzset();
    line("tzname"); str(tzname[0]); str(tzname[1]); field(timezone); field(daylight);
}

static void calls(void)
{
    int fds[2];
    char buffer[64];
    errno = 0;
    line("pipe"); field(pipe(fds)); field(write(fds[1], "through", 7)); field(read(fds[0], buffer, sizeof buffer)); bytes(buffer, 7);
    field(lseek(fds[0], 0, SEEK_SET)); error();
    int copy = dup(fds[0]);
    field(copy > 2); field(fcntl(copy, F_GETFD)); field(fcntl(copy, F_SETFD, FD_CLOEXEC)); field(fcntl(copy, F_GETFD));
    field(close(copy)); field(close(fds[0])); field(close(fds[1]));
    line("bad fds"); field(close(-1)); error(); field(isatty(-1)); error(); field(read(-1, buffer, 1)); error();
    line("open"); field(open("/nonexistent/file", O_RDONLY)); error();
    struct stat st;
    line("stat"); field(stat("/", &st)); field(S_ISDIR(st.st_mode)); field(stat("/nonexistent", &st)); error();
    field(lstat("/", &st)); field(S_ISDIR(st.st_mode));
    int root = open("/", O_RDONLY | O_DIRECTORY);
    field(fstat(root, &st)); field(S_ISDIR(st.st_mode)); field(close(root));
    line("access"); field(access("/", R_OK)); field(access("/nonexistent", F_OK)); error();
    line("getcwd"); field(getcwd(buffer, 1) == NULL); error();
    char *cwd = getcwd(NULL, 0);
    char full[4096];
    field(cwd != NULL); field(!strcmp(cwd, getcwd(full, sizeof full))); free(cwd);
    field(getcwd(NULL, 1) == NULL); error();
    line("sysconf"); field(sysconf(_SC_PAGESIZE)); field(getpagesize()); field(sysconf(-1)); error();
    struct utsname names;
    line("uname"); field(uname(&names)); str(names.sysname);
    line("ids"); field(getpid() == getpid()); field(getuid() == geteuid()); field(sched_yield());
}

static int by_value(const void *a, const void *b)
{
    int x = *(const int *)a, y = *(const int *)b;
    return (x > y) - (x < y);
}

static int by_value_reversed(const void *a, const void *b, void *direction)
{
    return *(int *)direction * by_value(a, b);
}

struct named { const char *name; int rank; };

static int by_name(const void *a, const void *b)
{
    return strcmp(((const struct named *)a)->name, ((const struct named *)b)->name);
}

static void sorting(void)
{
    int values[50];
    for (int i = 0; i < 50; i++) values[i] = (i * 37 + 11) % 101 - 50;
    qsort(values, 50, sizeof *values, by_value);
    line("qsort"); for (int i = 0; i < 50; i++) field(values[i]);
    int direction = -1;
    qsort_r(values, 50, sizeof *values, by_value_reversed, &direction);
    line("qsort_r"); for (int i = 0; i < 50; i++) field(values[i]);
    qsort(values, 0, sizeof *values, by_value);
    qsort(values, 1, sizeof *values, by_value);
    line("qsort small"); field(values[0]);
    /* Pointers keep their capabilities through the swaps. */
    struct named people[] = { { "mallory", 3 }, { "alice", 1 }, { "eve", 5 }, { "bob", 2 }, { "carol", 4 } };
    qsort(people, 5, sizeof *people, by_name);
    line("qsort pointers"); for (int i = 0; i < 5; i++) { str(people[i].name); field(people[i].rank); }
    int sorted[] = { 1, 3, 5, 7, 9, 11 };
    for (int key = 0; key < 13; key++) {
        int *found = bsearch(&key, sorted, 6, sizeof *sorted, by_value);
        line("bsearch"); field(key); at(found, sorted);
    }
}

static void sets(void)
{
    sigset_t set, other, result;
    errno = 0;
    line("sigemptyset"); field(sigemptyset(&set)); field(sigisemptyset(&set));
    for (int signal = 0; signal < 70; signal += 3) { field(sigaddset(&set, signal)); error(); }
    for (int signal = 0; signal < 70; signal++) field(sigismember(&set, signal));
    error();
    line("sigdelset"); field(sigdelset(&set, 3)); field(sigismember(&set, 3)); field(sigdelset(&set, 99)); error();
    line("sigfillset"); field(sigfillset(&other)); for (int signal = 1; signal < 65; signal++) field(sigismember(&other, signal));
    line("sigandset"); field(sigandset(&result, &set, &other)); for (int signal = 1; signal < 65; signal++) field(sigismember(&result, signal));
    sigemptyset(&other); sigaddset(&other, SIGUSR1);
    line("sigorset"); field(sigorset(&result, &set, &other)); for (int signal = 1; signal < 65; signal++) field(sigismember(&result, signal));
    line("sigisemptyset"); field(sigisemptyset(&result));
}

static void wide_str(const wchar_t *s)
{
    text(" L\"");
    for (; *s; s++) { char c[2] = { *s < 128 ? (char)*s : '?', 0 }; text(c); }
    text("\"");
}

static void locales(void)
{
    line("setlocale"); str(setlocale(LC_ALL, NULL)); str(setlocale(LC_CTYPE, "C")); str(setlocale(LC_ALL, "POSIX"));
    field(setlocale(LC_ALL, "xx_YY.nonexistent") == NULL);
    locale_t c = newlocale(LC_ALL_MASK, "C", (locale_t)0);
    line("newlocale"); field(c != (locale_t)0);
    field(newlocale(LC_ALL_MASK, "xx_YY.nonexistent", (locale_t)0) == (locale_t)0); error();
    line("uselocale"); field(uselocale((locale_t)0) == LC_GLOBAL_LOCALE);
    locale_t old = uselocale(c); field(old == LC_GLOBAL_LOCALE); field(uselocale((locale_t)0) == c);
    uselocale(old); field(uselocale((locale_t)0) == LC_GLOBAL_LOCALE);
    line("_l"); field(isalpha_l('a', c) != 0); field(isdigit_l('a', c) != 0); field(toupper_l('q', c)); field(tolower_l('Q', c));
    field(strtol_l("  -42z", NULL, 10, c)); field((long long)(strtod_l("2.5", NULL, c) * 10)); sign(strcoll_l("a", "b", c));
    char buffer[32];
    struct tm tm = { .tm_year = 100, .tm_mon = 1, .tm_mday = 3 };
    field(strftime_l(buffer, sizeof buffer, "%Y-%m-%d", &tm, c)); str(buffer);
    freelocale(c);
    struct lconv *conv = localeconv();
    line("localeconv"); str(conv->decimal_point); str(conv->thousands_sep); str(conv->grouping); field(conv->int_frac_digits == CHAR_MAX);
    line("nl_langinfo"); str(nl_langinfo(CODESET)); str(nl_langinfo(RADIXCHAR)); str(nl_langinfo(D_FMT));
    line("mb_cur_max"); field(MB_CUR_MAX);
}

static void wides(void)
{
    const wchar_t *hello = L"hello, wide";
    line("wcslen"); field(wcslen(hello)); field(wcsnlen(hello, 3));
    line("wcscmp"); sign(wcscmp(L"abc", L"abd")); sign(wcsncmp(L"abc", L"abd", 2)); sign(wcscmp(L"b", L"a"));
    line("wcschr"); at(wcschr(hello, L','), hello); at(wcsrchr(hello, L'e'), hello); at(wcschr(hello, L'z'), hello);
    wchar_t buffer[32];
    line("wcscpy"); wcscpy(buffer, L"copy"); wcscat(buffer, L"+cat"); wide_str(buffer);
    wcsncpy(buffer, L"ab", 4); field(buffer[2]); field(buffer[3]);
    line("wmem"); wmemset(buffer, L'x', 3); buffer[3] = 0; wide_str(buffer);
    at(wmemchr(hello, L'w', 11), hello); sign(wmemcmp(L"abc", L"abd", 3));
    wmemcpy(buffer, L"0123456789", 11); wmemmove(buffer + 2, buffer, 5); wide_str(buffer);
    wchar_t *copy = wcsdup(hello); wide_str(copy); free(copy);
    wchar_t *end;
    line("wcstol"); field(wcstol(L"  -123xyz", &end, 10)); field(end - (wchar_t *)L"  -123xyz" > 0);
    field(wcstoul(L"ff", NULL, 16)); field(wcstoll(L"0x10", NULL, 0)); field((long long)(wcstod(L"3.25", NULL) * 100));
    line("btowc"); field(btowc('a')); field(btowc(EOF) == WEOF); field(wctob(L'b')); field(wctob(0x263a));
    mbstate_t state = { 0 };
    wchar_t unit = 0;
    line("mbrtowc"); field((long long)mbrtowc(&unit, "z", 1, &state)); field(unit); field((long long)mbrtowc(&unit, "", 1, &state));
    field((long long)mbrtowc(&unit, "\xc3\xa9", 2, &state)); error();
    memset(&state, 0, sizeof state);
    line("mbtowc"); field(mbtowc(&unit, "q", 1)); field(unit); field(mblen("ab", 2)); field(mbrlen("x", 1, &state));
    char bytes[16] = { 0 };
    line("wcrtomb"); field((long long)wcrtomb(bytes, L'k', &state)); field(bytes[0]); field((long long)wcrtomb(bytes, 0x263a, &state)); error();
    line("mbsinit"); field(mbsinit(&state) != 0); field(mbsinit(NULL) != 0);
    const char *source = "convert me";
    line("mbsrtowcs"); field((long long)mbsrtowcs(buffer, &source, 32, &state)); wide_str(buffer); field(source == NULL);
    source = "partial";
    field((long long)mbsnrtowcs(buffer, &source, 4, 32, &state)); field(source ? source - "partial" : -1);
    source = "count";
    field((long long)mbsrtowcs(NULL, &source, 0, &state));
    const wchar_t *wsource = L"back again";
    line("wcsrtombs"); field((long long)wcsrtombs(bytes, &wsource, sizeof bytes, &state)); str(bytes); field(wsource == NULL);
    wsource = L"limited";
    memset(bytes, 0, sizeof bytes);
    field((long long)wcsnrtombs(bytes, &wsource, 3, sizeof bytes, &state)); str(bytes);
    line("mbstowcs"); field((long long)mbstowcs(buffer, "many", 32)); wide_str(buffer); field((long long)wcstombs(bytes, L"few", sizeof bytes)); str(bytes);
    line("wctype");
    for (wint_t c = 0; c < 128; c += 7)
        { field(!!iswalpha(c)); field(!!iswdigit(c)); field(!!iswspace(c)); field(!!iswpunct(c)); field(towupper(c)); }
    field(!!iswctype(L'a', wctype("alpha"))); field(!!iswctype(L'1', wctype("alpha"))); field(towctrans(L'a', wctrans("toupper")));
    locale_t c = newlocale(LC_ALL_MASK, "C", (locale_t)0);
    field(!!iswalpha_l(L'a', c)); field(towupper_l(L'b', c));
    freelocale(c);
    line("swprintf"); field(swprintf(buffer, 32, L"%d-%s-%ls %.2f", 7, "narrow", L"wide", 1.5)); wide_str(buffer);
    field(swprintf(buffer, 4, L"%s", "too long"));
}

static void scanning(void)
{
    int a = 0, b = 0, n = 0;
    unsigned u = 0;
    long l = 0;
    long long ll = 0;
    short h = 0;
    char c = 0, word[16] = { 0 }, set[16] = { 0 }, chars[4] = { 0 };
    float f = 0;
    double d = 0;
    long double ld = 0;
    void *p = NULL;
    line("sscanf ints"); field(sscanf("12 -34", "%d %d", &a, &b)); field(a); field(b);
    field(sscanf("0x1f 017 99", "%i %i %u", &a, &b, &u)); field(a); field(b); field(u);
    field(sscanf("ff 777", "%x %o", &a, &b)); field(a); field(b);
    field(sscanf("123456789012 -5 70000", "%lld %ld %hd", &ll, &l, &h)); field(ll); field(l); field(h);
    line("sscanf width"); field(sscanf("123456", "%3d%2d", &a, &b)); field(a); field(b);
    line("sscanf floats"); field(sscanf("1.5 2.25e2 -0.125", "%f %lf %Lf", &f, &d, &ld)); field((long long)(f * 100)); field((long long)d); field((long long)(ld * 1000));
    line("sscanf strings"); field(sscanf("  hello world", "%s %c", word, &c)); str(word); field(c);
    field(sscanf("abc123def", "%[a-z]%n", set, &n)); str(set); field(n);
    field(sscanf("xyz!", "%[^!]", set)); str(set);
    field(sscanf("abcd", "%3c", chars)); field(chars[0]); field(chars[2]);
    line("sscanf literal"); field(sscanf("key=42;", "key=%d;", &a)); field(a); field(sscanf("100%", "%d%%", &a)); field(a);
    line("sscanf suppress"); field(sscanf("1 2 3", "%*d %d %*d", &a)); field(a);
    line("sscanf failures"); a = 77; field(sscanf("abc", "%d", &a)); field(a); field(sscanf("", "%d", &a)); field(sscanf("   ", "%d", &a));
    field(sscanf("5 x", "%d %d", &a, &b)); field(a);
    line("sscanf pointer"); field(sscanf("0x1234", "%p", &p)); field((long long)(uintptr_t)p);
}

size_t transcript_length(void)
{
    if (!used) {
        strings();
        classes();
        numbers();
        allocation();
        errors();
        environment();
        times();
        calls();
        sorting();
        sets();
        locales();
        wides();
        scanning();
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

/* The environment, shared with Rust. */
int set_from_c(void) { return setenv("FIL_RUST_FROM_C", "c-value", 1); }

int sees_from_rust(void)
{
    const char *value = getenv("FIL_RUST_FROM_RUST");
    return value && !strcmp(value, "rust-value");
}

int environ_sees_from_rust(void) { return in_environ("FIL_RUST_FROM_RUST=rust-value"); }
