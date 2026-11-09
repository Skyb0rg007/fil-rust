#include <errno.h>
#include <regex.h>
#include <stdint.h>
#include <unistd.h>

int set_errno_to(int value) { errno = value; return errno; }
int get_errno(void) { return errno; }
uintptr_t errno_address(void) { return (uintptr_t)&errno; }

/* revoke and re_syntax_options have nothing behind them. */
int revoke_is_there(void) { int (*volatile function)(const char*) = revoke; return function != 0; }
int call_revoke(void) { return revoke("/nonexistent"); }
long read_re_syntax_options(void) { return (long)re_syntax_options; }
