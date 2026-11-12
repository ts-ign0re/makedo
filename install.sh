#!/bin/sh
# makedo installer: curl -fsSL https://raw.githubusercontent.com/ts-ign0re/makedo/main/install.sh | sh
set -eu

REPO="ts-ign0re/makedo"
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  TARGET=x86_64-unknown-linux-musl ;;  # статический — работает везде
  Darwin-x86_64) TARGET=x86_64-apple-darwin ;;
  Darwin-arm64)  TARGET=aarch64-apple-darwin ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

TAG=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name"' | cut -d'"' -f4)
URL="https://github.com/$REPO/releases/download/$TAG/makedo-$TARGET.tar.gz"
DEST="${MAKEDO_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$DEST"
echo "installing makedo $TAG ($TARGET) to $DEST"
curl -fsSL "$URL" | tar xz -C "$DEST"
chmod +x "$DEST/makedo"
# macOS инвалидирует подпись при распаковке/копировании → SIGKILL при запуске.
# Переставляем ad-hoc подпись (Linux — no-op, codesign отсутствует).
if command -v codesign >/dev/null 2>&1; then
  codesign -s - -f "$DEST/makedo" >/dev/null 2>&1 || true
fi
"$DEST/makedo" --version
echo "done. Ensure $DEST is in your PATH."
