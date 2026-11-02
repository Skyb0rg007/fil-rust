/* Stand-in for the yolo glibc's libm of Fil-C's /opt/fil distribution
   (libyolomimpl.so). The runtime imports a few of glibc's internal
   long-double kernels; the public functions compute the same values. */
#define _GNU_SOURCE
#include <math.h>

long double __ieee754_exp10l(long double x) { return exp10l(x); }
long double __ieee754_exp2l(long double x) { return exp2l(x); }
long double __ieee754_expl(long double x) { return expl(x); }
long double __ieee754_fmodl(long double x, long double y) { return fmodl(x, y); }
int __ieee754_ilogbl(long double x) { return ilogbl(x); }
long double __ieee754_log10l(long double x) { return log10l(x); }
long double __ieee754_log2l(long double x) { return log2l(x); }
long double __ieee754_logl(long double x) { return logl(x); }
long double __ieee754_powl(long double x, long double y) { return powl(x, y); }
long double __ieee754_remainderl(long double x, long double y) { return remainderl(x, y); }
long double __ieee754_scalbl(long double x, long double y) { return scalbl(x, y); }
long double __log10l_finite(long double x) { return log10l(x); }
