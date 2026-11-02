#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

/* A thread-safe word counter: a chained hash table behind a mutex. */
struct entry {
    char* word;
    long count;
    struct entry* next;
};

#define BUCKETS 1024
static struct entry* table[BUCKETS];
static pthread_mutex_t table_lock = PTHREAD_MUTEX_INITIALIZER;

static unsigned hash(const char* s, size_t n)
{
    unsigned h = 2166136261u;
    for (size_t i = 0; i < n; i++)
        h = (h ^ (unsigned char)s[i]) * 16777619u;
    return h % BUCKETS;
}

static void count_word(const char* s, size_t n)
{
    unsigned h = hash(s, n);
    pthread_mutex_lock(&table_lock);
    struct entry* e = table[h];
    while (e && !(strlen(e->word) == n && !memcmp(e->word, s, n)))
        e = e->next;
    if (!e) {
        e = malloc(sizeof *e);
        e->word = strndup(s, n);
        e->count = 0;
        e->next = table[h];
        table[h] = e;
    }
    e->count++;
    pthread_mutex_unlock(&table_lock);
}

/* Counts the space-separated words of `text[0..len)`. */
long count_words(const char* text, size_t len)
{
    long words = 0;
    size_t i = 0;
    while (i < len) {
        while (i < len && text[i] == ' ')
            i++;
        size_t start = i;
        while (i < len && text[i] != ' ')
            i++;
        if (i > start) {
            count_word(text + start, i - start);
            words++;
        }
    }
    return words;
}

long word_count(const char* word)
{
    unsigned h = hash(word, strlen(word));
    long n = 0;
    pthread_mutex_lock(&table_lock);
    for (struct entry* e = table[h]; e; e = e->next)
        if (!strcmp(e->word, word))
            n = e->count;
    pthread_mutex_unlock(&table_lock);
    return n;
}

/* A checksum of a byte buffer. */
uint64_t fnv1a(const unsigned char* data, size_t len)
{
    uint64_t h = 14695981039346656037ull;
    for (size_t i = 0; i < len; i++)
        h = (h ^ data[i]) * 1099511628211ull;
    return h;
}
