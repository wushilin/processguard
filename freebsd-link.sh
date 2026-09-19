#!/bin/sh
# Linker for hosts without cc/ld (pfSense): rust-lld plus the FreeBSD startup
# objects. Scrt1.o (which needs `main`) only goes into executables, not into
# shared objects such as proc-macros.
start=/usr/lib/Scrt1.o
for a in "$@"; do [ "$a" = "-shared" ] && start=; done
exec /usr/local/lib/rustlib/x86_64-unknown-freebsd/bin/rust-lld "$@" \
  $start /usr/lib/crti.o /usr/lib/crtbeginS.o /usr/lib/crtendS.o /usr/lib/crtn.o
