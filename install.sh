#!/bin/sh
# Install the cardio binary from the latest GitHub release — no cargo needed.
# Usage: curl -fsSL https://raw.githubusercontent.com/Red-CF/log-cardio/main/install.sh | sh
set -e

REPO=Red-CF/log-cardio

case "$(uname -s)" in
  Darwin) ci_os=macOS ;;
  Linux)  ci_os=Linux ;;
  Windows_NT|MINGW*|CYGWIN*)
    echo "Windows: download the .exe directly from https://github.com/$REPO/releases" >&2
    exit 1
    ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  arm64|aarch64) ci_arch=ARM64 ;;
  x86_64)        ci_arch=X64 ;;
  *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac

BIN="cardio-$ci_os-$ci_arch"
echo "downloading $BIN from the latest release ..."
curl -fsSL "https://github.com/$REPO/releases/latest/download/$BIN" -o "$BIN"
chmod +x "$BIN"

# where to put it: try /usr/local/bin, fall back to ~/.local/bin
if [ -w /usr/local/bin ]; then
  PREFIX=/usr/local/bin
else
  PREFIX="$HOME/.local/bin"
  mkdir -p "$PREFIX"
  case ":$PATH:" in *":$PREFIX:"*) ;; *) echo "note: add $PREFIX to your PATH" >&2 ;; esac
fi
mv "$BIN" "$PREFIX/cardio"
echo "installed: $PREFIX/cardio"
cardio --version 2>/dev/null || echo "(run it from the path above)"