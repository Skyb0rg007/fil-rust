/* stdio on the glibc flavor's libc. The transcript part is also built for the
 * host, and the two must match: the printf family, streams over files and
 * pipes, lines, positions, the inline getc and putc, and directories. The rest
 * is what only Fil-C checks: misuse, a scribbled-over FILE, and fork. */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <malloc.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#include <wchar.h>

static char out[1 << 16];
static size_t used;

size_t transcript_length(void) { return used; }
int transcript_byte(size_t index) { return (unsigned char)out[index]; }

static void text(const char *s) { while (*s && used < sizeof out) out[used++] = *s++; }
static void num(long long value)
{
    char digits[24];
    snprintf(digits, sizeof digits, "%lld", value);
    text(digits);
}
static void line(const char *name) { text("\n"); text(name); }
static void field(long long value) { text(" "); num(value); }
static void str(const char *s) { text(" \""); text(s ? s : "<null>"); text("\""); }
static void error(void) { text(" errno="); num(errno); errno = 0; }

/* One snprintf, its result and what it wrote. */
#define FORMAT(...)                                             \
    do {                                                        \
        char buffer[256];                                       \
        int count = snprintf(buffer, sizeof buffer, __VA_ARGS__); \
        line("format"); str(#__VA_ARGS__); field(count); str(buffer); \
    } while (0)

static int listed(char *buffer, size_t size, const char *format, ...)
{
    va_list list;
    va_start(list, format);
    int count = vsnprintf(buffer, size, format, list);
    va_end(list);
    return count;
}

static void formats(void)
{
    FORMAT("%d %i %u", -42, 17, 3000000000u);
    FORMAT("%x %X %o %#x %#o", 255, 255, 8, 255, 8);
    FORMAT("[%5d] [%-5d] [%05d] [%+d] [% d]", 42, 42, 42, 42, 42);
    FORMAT("[%.3d] [%8.3d] [%-8.3x]", 7, 7, 7);
    FORMAT("%hhd %hd %ld %lld", 300, 70000, -5000000000L, 123456789012345LL);
    FORMAT("%hhu %hu %lu %llu", 300, 70000, 18446744073709551615UL, 1ULL << 63);
    FORMAT("%zu %zd %jd %td", (size_t)99, (ssize_t)-99, (intmax_t)-7, (ptrdiff_t)12);
    FORMAT("%c%c%c", 'a', 'b', 'c');
    FORMAT("[%5c] [%-3c]", 'x', 'y');
    FORMAT("%s and %s", "this", "that");
    FORMAT("[%10s] [%-10s] [%.2s] [%10.3s]", "right", "left", "cut", "short");
    FORMAT("[%s] [%.3s] [%10s]", (char *)NULL, (char *)NULL, (char *)NULL);
    FORMAT("%f %F %.2f %.0f %#.0f", 3.14159, 2.5, 2.675, 2.5, 2.0);
    FORMAT("%e %E %.3e", 12345.678, 0.000123, -1.0);
    FORMAT("%g %G %g %g %#g", 0.0001, 1e-5, 123456789.0, 100.0, 1.0);
    FORMAT("%a %A %.2a", 1.0, -0.5, 3.0);
    FORMAT("[%10.3f] [%-10.2e] [%+.1f] [%010.2f]", 3.14159, 1234.5, 2.25, -1.5);
    /* Divided at run time: compilers fold 0.0 / 0.0 to NaNs of either sign. */
    static volatile double zero = 0.0;
    FORMAT("%f %f %f %e", 1.0 / zero, -1.0 / zero, zero / zero, -zero);
    FORMAT("%Lf %Le %Lg", 1.5L, 1024.0L, 0.25L);
    FORMAT("%*d|%-*d|%.*f", 6, 1, 4, 2, 3, 3.14159);
    FORMAT("%2$s %1$s %2$s", "world", "hello");
    FORMAT("%1$*2$d|%3$.*4$f", 5, 8, 2.5, 2);
    FORMAT("100%% sure");
    FORMAT("%'d %'.2f", 1234567, 1234567.891);
    FORMAT("%5%|%-5d|", 3);
    FORMAT("%lc %ls", (wint_t)'w', L"wide");

    char buffer[16];
    int count;
    line("snprintf truncates"); count = snprintf(buffer, 6, "%s", "abcdefghij"); field(count); str(buffer);
    line("snprintf zero"); field(snprintf(NULL, 0, "%d-%d", 12, 345));
    line("snprintf one"); buffer[0] = 'z'; field(snprintf(buffer, 1, "full")); field(buffer[0]);
    line("vsnprintf"); count = listed(buffer, sizeof buffer, "%s=%d", "v", 9); field(count); str(buffer);
    line("sprintf"); field(sprintf(buffer, "%03d|%c", 5, 'q')); str(buffer);
    char *made = NULL;
    line("asprintf"); field(asprintf(&made, "%s-%d", "made", 77)); str(made); free(made);
    int before = 0, after = 0;
    short half = 0;
    line("%n"); snprintf(buffer, sizeof buffer, "ab%ncdef%hn", &before, &half); field(before); field(half);
    snprintf(buffer, sizeof buffer, "%s%n", "xyz", &after); field(after);
    errno = ENOENT;
    line("%m"); snprintf(buffer, sizeof buffer, "%m"); str(buffer);
}

/* A stream's state, as the libc reports it. */
static void state(FILE *f) { field(feof(f)); field(ferror(f)); field(ftell(f)); }

static void streams(void)
{
    FILE *f = tmpfile();
    line("tmpfile"); field(f != NULL); field(fileno(f) > 2);
    line("fputs"); field(fputs("first line\n", f) >= 0);
    line("fprintf"); field(fprintf(f, "%s %d\n", "second", 2));
    line("fputc"); field(fputc('3', f)); field(putc('\n', f));
    line("fwrite"); field(fwrite("fourth\nno newline", 1, 17, f));
    line("fflush"); field(fflush(f)); state(f);
    rewind(f);
    line("rewind"); state(f);
    line("fgetc"); field(fgetc(f)); field(getc(f));
    line("ungetc"); field(ungetc('X', f)); field(fgetc(f)); field(ftell(f));
    char buffer[64];
    line("fgets"); str(fgets(buffer, sizeof buffer, f)); state(f);
    line("fgets short"); str(fgets(buffer, 4, f)); str(fgets(buffer, sizeof buffer, f));
    line("fread"); memset(buffer, 0, sizeof buffer); field(fread(buffer, 1, 2, f)); str(buffer);
    char *got = NULL;
    size_t size = 0;
    line("getline"); field(getline(&got, &size, f)); str(got); field(size >= 8);
    line("getline last"); field(getline(&got, &size, f)); str(got); state(f);
    line("getline end"); field(getline(&got, &size, f)); state(f);
    clearerr(f);
    line("clearerr"); state(f);
    line("fseek"); field(fseek(f, 6, SEEK_SET)); field(fgetc(f)); field(fseek(f, -2, SEEK_END)); field(fgetc(f));
    field(fseek(f, -100, SEEK_SET)); error();
    fpos_t position;
    line("fgetpos"); field(fseek(f, 3, SEEK_SET)); field(fgetpos(f, &position)); field(fgetc(f));
    field(fsetpos(f, &position)); field(fgetc(f));
    line("getdelim"); rewind(f); field(getdelim(&got, &size, ' ', f)); str(got);
    free(got);
    /* The inline getc and putc of glibc's headers. */
    rewind(f);
    line("inline getc");
    int c;
    long count = 0;
    while ((c = __getc_unlocked_body(f)) != EOF)
        count++;
    field(count); field(feof_unlocked(f)); field(ferror_unlocked(f));
    line("inline putc"); field(__putc_unlocked_body('!', f)); field(fflush(f)); field(ftell(f));
    line("fclose"); field(fclose(f));

    line("fopen missing"); f = fopen("/nonexistent/file", "r"); field(f == NULL); error();
    line("fopen bad mode"); f = fopen("/dev/null", "q"); field(f == NULL); error();
    line("read write-only"); f = fopen("/dev/null", "w"); field(fgetc(f)); field(ferror(f)); fclose(f);

    line("popen"); f = popen("echo from a pipe; exit 3", "r");
    str(fgets(buffer, sizeof buffer, f)); field(fgetc(f)); field(feof(f));
    int status = pclose(f); field(WIFEXITED(status)); field(WEXITSTATUS(status));

    f = tmpfile();
    line("dprintf"); field(dprintf(fileno(f), "<%d>", 5)); fflush(f); rewind(f); str(fgets(buffer, sizeof buffer, f));
    line("fdopen"); FILE *again = fdopen(dup(fileno(f)), "r"); field(again != NULL); rewind(again);
    str(fgets(buffer, sizeof buffer, again)); field(fclose(again)); fclose(f);

    line("setvbuf"); field(setvbuf(stdout, NULL, _IOLBF, 0)); field(setvbuf(stdout, NULL, 99, 0)); error();
    line("std fileno"); field(fileno(stdin)); field(fileno(stdout)); field(fileno(stderr));
}

static int compare(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

static void directories(const char *path)
{
    char name[512];
    mkdir(path, 0700);
    const char *entries[] = { "beta", "alpha", "gamma.txt" };
    for (int i = 0; i < 3; i++) {
        snprintf(name, sizeof name, "%s/%s", path, entries[i]);
        if (i == 0)
            mkdir(name, 0700);
        else
            fclose(fopen(name, "w"));
    }
    DIR *dir = opendir(path);
    line("opendir"); field(dir != NULL); field(dirfd(dir) > 2);
    char *names[8];
    int count = 0, directories = 0;
    long middle = -1;
    struct dirent *entry;
    while ((entry = readdir(dir)) && count < 8) {
        if (count == 2)
            middle = telldir(dir);
        directories += entry->d_type == DT_DIR;
        names[count++] = strdup(entry->d_name);
    }
    qsort(names, count, sizeof *names, compare);
    line("readdir"); field(count); field(directories);
    for (int i = 0; i < count; i++) {
        str(names[i]);
        free(names[i]);
    }
    line("seekdir"); seekdir(dir, middle); int rest = 0;
    while (readdir(dir))
        rest++;
    field(rest);
    line("rewinddir"); rewinddir(dir); rest = 0;
    while (readdir(dir))
        rest++;
    field(rest);
    line("closedir"); field(closedir(dir));
    line("opendir missing"); field(opendir("/nonexistent/dir") == NULL); error();
    for (int i = 0; i < 3; i++) {
        snprintf(name, sizeof name, "%s/%s", path, entries[i]);
        remove(name);
    }
    rmdir(path);
}

void write_transcript(const char *directory)
{
    used = 0;
    formats();
    streams();
    directories(directory);
    text("\n");
}

/* Fil-C only from here. */

/* Output interleaved with Rust's println!. */
void print_some(int which)
{
    if (which == 0) {
        printf("c %d\n", 1);
        fputs("c partial ", stdout);
        puts("then puts");
    } else {
        putchar('c');
        fprintf(stdout, " %s", "two");
        fwrite("\n", 1, 1, stdout);
    }
}

/* Writes over every field of a FILE: the libc goes by which object it is. */
int scribbled_file_still_works(void)
{
    FILE *f = tmpfile();
    int fd = fileno(f);
    char mine[8];
    f->_flags = 0;
    f->_fileno = 12345;
    f->_IO_read_ptr = f->_IO_read_end = f->_IO_read_base = NULL;
    f->_IO_buf_base = f->_IO_buf_end = NULL;
    f->_IO_write_base = f->_IO_write_ptr = mine;
    f->_IO_write_end = mine;
    f->_chain = f;
    f->_lock = NULL;
    f->_mode = -1;
    if (fputs("kept", f) < 0 || fileno(f) != fd || fflush(f) != 0)
        return 1;
    rewind(f);
    char buffer[8] = { 0 };
    if (!fgets(buffer, sizeof buffer, f) || strcmp(buffer, "kept"))
        return 2;
    return fclose(f);
}

/* A FILE-shaped object the libc didn't open. */
int fake_file(void)
{
    FILE *f = calloc(1, sizeof(FILE));
    f->_flags = 0xfbad0000;
    f->_fileno = 1;
    return fputs("forged", f);
}

int use_after_fclose(void)
{
    FILE *f = tmpfile();
    fclose(f);
    return fputc('x', f);
}

int read_after_fclose(void)
{
    FILE *f = tmpfile();
    fclose(f);
    return feof_unlocked(f);
}

int entry_after_closedir(void)
{
    DIR *dir = opendir("/");
    struct dirent *entry = readdir(dir);
    closedir(dir);
    return entry->d_name[0];
}

/* Bytes with no terminator anywhere in their object. */
static char *unterminated(size_t *size)
{
    char *bytes = malloc(5);
    *size = malloc_usable_size(bytes);
    memset(bytes, 'a', *size);
    return bytes;
}

/* %s stops at the precision or the object's end, whichever comes first. */
int capped_string(void)
{
    size_t size;
    char *bytes = unterminated(&size);
    char buffer[256];
    int count = snprintf(buffer, sizeof buffer, "[%.3s][%.200s]", bytes, bytes);
    if (count != (int)size + 7 || strncmp(buffer, "[aaa][aaaa", 10) || buffer[count - 1] != ']')
        return -1;
    free(bytes);
    return count - (int)size;
}

/* With no precision, it needs a terminator. */
int unterminated_string(void)
{
    size_t size;
    char *bytes = unterminated(&size);
    char buffer[256];
    return snprintf(buffer, sizeof buffer, "%s", bytes);
}

/* %lln stores 8 bytes, into 4. */
int count_too_wide(void)
{
    char *bytes = malloc(4);
    char *last = bytes + malloc_usable_size(bytes) - 4;
    char buffer[8];
    return snprintf(buffer, sizeof buffer, "ab%lln", (long long *)last);
}

int count_through_an_integer(void)
{
    char buffer[8];
    return snprintf(buffer, sizeof buffer, "ab%n", (int *)(uintptr_t)0x1000);
}

int sprintf_overflows(void)
{
    char *small = malloc(4);
    return sprintf(small, "%s", "much too long for it");
}

#pragma clang diagnostic ignored "-Wformat-insufficient-args"
int too_few_arguments(void)
{
    char buffer[32];
    return snprintf(buffer, sizeof buffer, "%d %d %s");
}

/* A stream with buffered output survives a fork: each process writes and
 * closes its own copy, as with any libc. */
int fork_with_buffered_streams(const char *path)
{
    FILE *f = fopen(path, "w");
    if (!f)
        return -1;
    fputs("before ", f);
    printf("parent before fork\n");
    pid_t child = fork();
    if (child < 0)
        return -2;
    if (child == 0) {
        fputs("child\n", f);
        fclose(f);
        printf("child printed\n");
        fflush(stdout);
        _exit(0);
    }
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status))
        return -3;
    fputs("parent\n", f);
    return fclose(f);
}
