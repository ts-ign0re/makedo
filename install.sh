#!/bin/sh
# makedo installer
#   CLI:               curl -fsSL https://raw.githubusercontent.com/ts-ign0re/makedo/main/install.sh | sh
#   CLI + macOS app:   curl -fsSL https://raw.githubusercontent.com/ts-ign0re/makedo/main/install.sh | sh -s -- --app
set -eu

REPO="ts-ign0re/makedo"
APP=0
for arg in "$@"; do
  case "$arg" in
    --app) APP=1 ;;
    *) echo "unknown option: $arg (supported: --app)" >&2; exit 1 ;;
  esac
done

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)                TARGET=x86_64-unknown-linux-musl ;;   # статический — работает везде
  Linux-aarch64 | Linux-arm64) TARGET=aarch64-unknown-linux-musl ;;
  Darwin-x86_64)               TARGET=x86_64-apple-darwin ;;
  Darwin-arm64)                TARGET=aarch64-apple-darwin ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac
if [ "$APP" = 1 ] && [ "$(uname -s)" != Darwin ]; then
  echo "--app is only available on macOS" >&2
  exit 1
fi

# releases/latest/download вместо api.github.com: у API лимит 60 запросов в час на IP
BASE="${MAKEDO_RELEASE_URL:-https://github.com/$REPO/releases/latest/download}"
DEST="${MAKEDO_INSTALL_DIR:-$HOME/.local/bin}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
if command -v sha256sum >/dev/null 2>&1; then SUM="sha256sum"; else SUM="shasum -a 256"; fi

# скачивает файл релиза в $TMP и сверяет его контрольную сумму
fetch() {
  echo "downloading $1"
  curl -fsSL -o "$TMP/$1" "$BASE/$1"
  curl -fsSL -o "$TMP/$1.sha256" "$BASE/$1.sha256"
  (cd "$TMP" && $SUM -c "$1.sha256" >/dev/null) || { echo "checksum mismatch for $1" >&2; exit 1; }
}

fetch "makedo-$TARGET.tar.gz"
mkdir -p "$DEST"
tar xzf "$TMP/makedo-$TARGET.tar.gz" -C "$DEST"
chmod +x "$DEST/makedo"
# macOS инвалидирует подпись при распаковке/копировании → SIGKILL при запуске.
# Переставляем ad-hoc подпись (Linux — no-op, codesign отсутствует).
if command -v codesign >/dev/null 2>&1; then
  codesign -s - -f "$DEST/makedo" >/dev/null 2>&1 || true
fi
"$DEST/makedo" --version
echo "installed makedo to $DEST. Ensure it is in your PATH."

if [ "$APP" = 1 ]; then
  fetch "Makedo-$TARGET.app.tar.gz"
  APPS="${MAKEDO_APP_DIR:-/Applications}"
  # без прав на /Applications ставим в личную папку программ
  if [ -z "${MAKEDO_APP_DIR:-}" ] && [ ! -w "$APPS" ]; then APPS="$HOME/Applications"; fi
  mkdir -p "$APPS"
  tar xzf "$TMP/Makedo-$TARGET.app.tar.gz" -C "$TMP"
  rm -rf "$APPS/Makedo.app"   # прежняя версия приложения, если была
  mv "$TMP/Makedo.app" "$APPS/Makedo.app"
  codesign --force --deep -s - "$APPS/Makedo.app" >/dev/null 2>&1 || true
  echo "installed Makedo.app to $APPS"
fi
