// Embedded by tests/embedded.rs. The counter shows whether two loads share
// one copy of the library.
static int count;

int embed_bump(void) { return ++count; }
