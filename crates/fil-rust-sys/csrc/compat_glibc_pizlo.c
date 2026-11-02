/* A name-only stand-in for "libpizlo.so".

   The glibc distribution's libpizlo.so has no DT_SONAME, and its user
   libraries carry no RUNPATH, so their DT_NEEDED "libpizlo.so" would not
   match the real runtime that fil-rust loads by path. This empty object has
   that soname, satisfying the dependency by name; the runtime's symbols come
   from the real libpizlo.so, which is loaded RTLD_GLOBAL. */
int fil_rust_pizlo_stand_in;
