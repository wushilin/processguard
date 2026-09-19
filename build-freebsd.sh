#!/bin/sh
# Builds on a FreeBSD host without cc/ld (pfSense), linking through
# freebsd-link.sh. The env vars below take precedence over any rustflags and
# linker in ~/.cargo/config. Extra arguments go to cargo build.
set -e
cd "$(dirname "$0")"
export PATH=/usr/local/bin:$PATH
export CARGO_TARGET_X86_64_UNKNOWN_FREEBSD_LINKER=$PWD/freebsd-link.sh
export RUSTFLAGS="-C linker-flavor=ld.lld -C link-arg=--dynamic-linker=/libexec/ld-elf.so.1 -C link-arg=-L/usr/lib -C link-arg=-L/lib"
cargo build --release "$@"
ls -l target/release/processguard
