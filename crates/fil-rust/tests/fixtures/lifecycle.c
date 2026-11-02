/* For tests that end the process while Fil-C code is busy. */
#include <unistd.h>

volatile long spins;

/* Never returns. */
void spin_forever(void) {
    for (;;) spins++;
}

void sleep_forever(void) {
    for (;;) sleep(1);
}
