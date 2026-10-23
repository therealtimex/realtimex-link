#!/bin/bash
# Build RealTimeX Link's executables (rtx-linkd, rtx-link) for macOS or Linux, release profile,
# into OUTPUT_DIR. Usage: scripts/build.sh [arm64|x86_64] [output_dir]
# Rust comes from rustup (the version in rust-toolchain.toml).
set -euo pipefail
ARCH_PARAM="${1:-$(uname -m)}"
OUTPUT_DIR="${2:-dist}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ "$OUTPUT_DIR" != /* ]] && OUTPUT_DIR="$PWD/$OUTPUT_DIR"
case "$ARCH_PARAM" in
  arm64 | aarch64) CPU="aarch64" ;;
  x86_64 | amd64) CPU="x86_64" ;;
  *) echo "ERROR: architecture must be arm64 or x86_64" >&2; exit 1 ;;
esac
case "$(uname -s)" in
  Darwin) TARGET="$CPU-apple-darwin" ;;
  Linux) TARGET="$CPU-unknown-linux-gnu" ;;
  *) echo "ERROR: use scripts/build.ps1 on Windows" >&2; exit 1 ;;
esac
export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo "ERROR: Rust (rustup) is not installed on this machine" >&2; exit 1; }
# RealTimeX ships for glibc 2.34 and later (glibc/glibc_compat.c covers the newer symbols).
GLIBC_MAX=2.34

cd "$ROOT"
rustup target add "$TARGET" >/dev/null
echo "Building rtx-linkd and rtx-link for $TARGET into $OUTPUT_DIR"
cargo build --release --locked --target "$TARGET" -p rtx-linkd
cargo build --release --locked --target "$TARGET" -p rtx-link
mkdir -p "$OUTPUT_DIR"
for bin in rtx-linkd rtx-link; do
  BIN="target/$TARGET/release/$bin"
  if [ "$(uname -s)" = Linux ] && command -v objdump >/dev/null 2>&1; then
    NEED=$(objdump -p "$BIN" | sed -n '/^Version References:/,$p' | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)
    echo "$bin requires glibc $NEED"
    if [ "$(printf '%s\n%s\n' "$NEED" "$GLIBC_MAX" | sort -V | tail -1)" != "$GLIBC_MAX" ]; then
      echo "ERROR: $bin requires glibc $NEED, more than $GLIBC_MAX" >&2
      exit 1
    fi
  fi
  cp "$BIN" "$OUTPUT_DIR/$bin"
  chmod +x "$OUTPUT_DIR/$bin"
done
"$OUTPUT_DIR/rtx-linkd" --version
echo "SUCCESS: $OUTPUT_DIR/rtx-linkd $OUTPUT_DIR/rtx-link"
