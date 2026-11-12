#!/bin/sh
# makedo installer: curl -fsSL https://raw.githubusercontent.com/ts-ign0re/makedo/main/install.sh | sh
set -eu

REPO="ts-ign0re/makedo"
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)                TARGET=x86_64-unknown-linux-musl ;;   # статический — работает везде
  Linux-aarch64 | Linux-arm64) TARGET=aarch64-unknown-linux-musl ;;
  Darwin-x86_64)               TARGET=x86_64-apple-darwin ;;
  Darwin-arm64)                TARGET=aarch64-apple-darwin ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

# releases/latest/download вместо api.github.com: у API лимит 60 запросов в час на IP
BASE="${MAKEDO_RELEASE_URL:-https://github.com/$REPO/releases/latest/download}"
FILE="makedo-$TARGET.tar.gz"
DEST="${MAKEDO_INSTALL_DIR:-$HOME/.local/bin}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "downloading $FILE"
curl -fsSL -o "$TMP/$FILE" "$BASE/$FILE"
curl -fsSL -o "$TMP/$FILE.sha256" "$BASE/$FILE.sha256"
if command -v sha256sum >/dev/null 2>&1; then SUM="sha256sum"; else SUM="shasum -a 256"; fi
(cd "$TMP" && $SUM -c "$FILE.sha256" >/dev/null) || { echo "checksum mismatch for $FILE" >&2; exit 1; }

mkdir -p "$DEST"
tar xzf "$TMP/$FILE" -C "$DEST"
chmod +x "$DEST/makedo"
# macOS инвалидирует подпись при распаковке/копировании → SIGKILL при запуске.
# Переставляем ad-hoc подпись (Linux — no-op, codesign отсутствует).
if command -v codesign >/dev/null 2>&1; then
  codesign -s - -f "$DEST/makedo" >/dev/null 2>&1 || true
fi
"$DEST/makedo" --version
echo "installed to $DEST. Ensure it is in your PATH."
