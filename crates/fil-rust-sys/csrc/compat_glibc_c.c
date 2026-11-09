/* Stand-in for the yolo glibc of Fil-C's /opt/fil distribution
   (libyolocimpl.so) on top of the host glibc.

   Fil-C objects bind the few yolo-libc symbols they import (memset,
   __tls_get_addr and the like) straight to the host glibc: glibc's dynamic
   linker matches versioned references by version name, in any object of the
   global scope. This library supplies only the soname and the version
   definitions (compat_glibc_c.map). */
int fil_rust_libc_stand_in;
