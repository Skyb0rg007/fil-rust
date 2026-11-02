/* Stand-in for the dynamic loader of Fil-C's /opt/fil distribution
   (ld-fil1-x86_64.so). Fil-C objects only need its soname and version
   definitions (compat_glibc_ld.map); the symbols they import from it, like
   __tls_get_addr, bind to the host glibc's loader. */
int fil_rust_ld_stand_in;
