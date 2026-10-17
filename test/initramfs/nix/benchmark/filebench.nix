{ filebench, }:
filebench.overrideAttrs (old: {
  postPatch = (old.postPatch or "") + ''
    substituteInPlace fileset.c \
      --replace-fail "fb_strlcpy(s, path, MAXPATHLEN);" \
                     "fb_strlcpy(s, path, strlen(path) + 1);"
    substituteInPlace Makefile.am \
      --replace-fail "bin_PROGRAMS = filebench" \
                     $'bin_PROGRAMS = filebench\nfilebench_LDFLAGS = -no-pie'
  '';
  postConfigure = (old.postConfigure or "") + ''
    substituteInPlace config.h \
      --replace-fail "#define HAVE_PTHREAD_MUTEXATTR_SETPROTOCOL 1" \
                     "/* #undef HAVE_PTHREAD_MUTEXATTR_SETPROTOCOL */"
  '';
})
