#!/bin/bash
# Build RealTimeX Link's executables (rtx-linkd, rtx-link, rtx-control) for macOS or Linux, release
# profile, into OUTPUT_DIR: on macOS as RealTimeX Link.app (scripts/bundle-macos.sh), on Linux as
# the three executables. Usage: scripts/build.sh [arm64|x86_64] [output_dir]
# Rust comes from rustup (the version in rust-toolchain.toml).
# LINK_AGENT_DESKTOP_DIR: the pinned agent-desktop CLI for this target (ci/fetch_agent_desktop.py,
# realtimex-ai-app#3055), put next to rtx-control. Without it the build has no accessibility layer
# (rtx-control answers `unavailable`).
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
# RealTimeX ships for glibc 2.34 and later (crates/glibc-compat/glibc_compat.c covers the newer
# symbols).
GLIBC_MAX=2.34

cd "$ROOT"
rustup target add "$TARGET" >/dev/null
echo "Building rtx-linkd, rtx-link and rtx-control for $TARGET into $OUTPUT_DIR"
cargo build --release --locked --target "$TARGET" -p rtx-linkd
cargo build --release --locked --target "$TARGET" -p rtx-link
# Each its own build, so none takes on another's dependency features.
cargo build --release --locked --target "$TARGET" -p rtx-control
BUILT="target/$TARGET/release"
mkdir -p "$OUTPUT_DIR"
for bin in rtx-linkd rtx-link rtx-control; do
  BIN="$BUILT/$bin"
  if [ "$(uname -s)" = Linux ] && command -v objdump >/dev/null 2>&1; then
    NEED=$(objdump -p "$BIN" | sed -n '/^Version References:/,$p' | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)
    echo "$bin requires glibc $NEED"
    if [ "$(printf '%s\n%s\n' "$NEED" "$GLIBC_MAX" | sort -V | tail -1)" != "$GLIBC_MAX" ]; then
      echo "ERROR: $bin requires glibc $NEED, more than $GLIBC_MAX. Functions above it:" >&2
      # Each needs a definition in crates/glibc-compat/glibc_compat.c.
      objdump -T "$BIN" | grep -o 'GLIBC_[0-9.]*)\? *[^ ]*$' | sed 's/GLIBC_//; s/)//' \
        | while read -r version name; do
            if [ "$(printf '%s\n%s\n' "$version" "$GLIBC_MAX" | sort -V | tail -1)" != "$GLIBC_MAX" ]; then
              echo "  $name (glibc $version)" >&2
            fi
          done
      exit 1
    fi
  fi
done
# The pinned agent-desktop files (not its .version note), when given.
AGENT_DESKTOP=()
if [ -n "${LINK_AGENT_DESKTOP_DIR:-}" ]; then
  for file in "$LINK_AGENT_DESKTOP_DIR"/agent-desktop*; do
    case "$file" in *.version) continue ;; esac
    [ -f "$file" ] && AGENT_DESKTOP+=("$file")
  done
  if [ ${#AGENT_DESKTOP[@]} -eq 0 ]; then
    echo "ERROR: LINK_AGENT_DESKTOP_DIR ($LINK_AGENT_DESKTOP_DIR) has no agent-desktop" >&2
    exit 1
  fi
  echo "agent-desktop $(cat "$LINK_AGENT_DESKTOP_DIR/agent-desktop.version" 2>/dev/null): ${AGENT_DESKTOP[*]##*/}"
else
  echo "No LINK_AGENT_DESKTOP_DIR: this build has no accessibility layer"
fi
if [ "$(uname -s)" = Darwin ]; then
  AGENT_DESKTOP_FILES="${AGENT_DESKTOP[*]+${AGENT_DESKTOP[*]}}" scripts/bundle-macos.sh "$BUILT" "$OUTPUT_DIR"
  OUT="$OUTPUT_DIR/RealTimeX Link.app/Contents/MacOS"
else
  for bin in rtx-linkd rtx-link rtx-control; do
    cp "$BUILT/$bin" "$OUTPUT_DIR/$bin"
    chmod +x "$OUTPUT_DIR/$bin"
  done
  for file in ${AGENT_DESKTOP[@]+"${AGENT_DESKTOP[@]}"}; do
    cp "$file" "$OUTPUT_DIR/"
    chmod +x "$OUTPUT_DIR/$(basename "$file")"
    # The prebuilt CLI must fit the same glibc floor (#3055 G1).
    if command -v objdump >/dev/null 2>&1; then
      NEED=$(objdump -T "$file" | { grep -o 'GLIBC_[0-9.]*' || true; } | sed 's/GLIBC_//' | sort -uV | tail -1)
      echo "$(basename "$file") requires glibc $NEED"
      if [ -n "$NEED" ] && [ "$(printf '%s\n%s\n' "$NEED" "$GLIBC_MAX" | sort -V | tail -1)" != "$GLIBC_MAX" ]; then
        echo "ERROR: $(basename "$file") requires glibc $NEED, more than $GLIBC_MAX" >&2
        exit 1
      fi
    fi
  done
  # The autostart entry's icon (rtx-linkd's autostart.rs) and the icon theme's sizes, which its
  # install.rs puts into hicolor for the menu entry and the dock.
  cp assets/icon-256.png "$OUTPUT_DIR/icon.png"
  mkdir -p "$OUTPUT_DIR/icons"
  for size in 16 24 32 48 64 128 256 512; do
    cp "assets/icon-$size.png" "$OUTPUT_DIR/icons/$size.png"
  done
  OUT="$OUTPUT_DIR"
fi
"$OUT/rtx-linkd" --version
"$OUT/rtx-control" --version
echo "SUCCESS: $OUT/rtx-linkd $OUT/rtx-link $OUT/rtx-control"
