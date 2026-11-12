#!/bin/sh
# Собирает target/Makedo.app: оконный makedo (`--features gui`) с иконкой из assets/icon.svg.
# usage: scripts/bundle-macos.sh
# ponytail: ad-hoc подпись, без нотаризации и .dmg — добавить, когда бандл пойдёт наружу.
set -eu
cd "$(dirname "$0")/.."
APP=target/Makedo.app
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

cargo build --profile dist --features gui

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/dist/makedo "$APP/Contents/MacOS/makedo"

# иконка: SVG → PNG 1024 с прозрачностью и полями macOS (AppKit читает SVG сам) → .icns
cat > "$TMP/svg2png.swift" <<'EOF'
import AppKit
let a = CommandLine.arguments
guard a.count == 3, let img = NSImage(contentsOfFile: a[1]) else { fatalError("usage: svg2png in.svg out.png") }
let px = 1024
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
    samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
rep.size = NSSize(width: px, height: px)
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
img.draw(in: NSRect(x: 100, y: 100, width: 824, height: 824))
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: a[2]))
EOF
swift "$TMP/svg2png.swift" assets/icon.svg "$TMP/1024.png"
mkdir "$TMP/Makedo.iconset"
for s in 16 32 128 256 512; do
  d=$((s * 2))
  sips -z "$s" "$s" "$TMP/1024.png" --out "$TMP/Makedo.iconset/icon_${s}x${s}.png" >/dev/null
  sips -z "$d" "$d" "$TMP/1024.png" --out "$TMP/Makedo.iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$TMP/Makedo.iconset" -o "$APP/Contents/Resources/Makedo.icns"

# символьный шрифт окна: JuliaMono (OFL) — брайль (спиннеры) и значок ветки, которых нет в Menlo.
# В репозиторий 3 MB не кладём: качаем один раз в target/ и сверяем sha256.
FONT=target/fonts/JuliaMono-Regular.ttf
FONT_URL=https://github.com/cormullion/juliamono/raw/v0.63.2
FONT_SHA=40a07da0d1601215eb6b89312eb44128a3e2f36675d3e1f518264bd391fc7023
if [ ! -f "$FONT" ]; then
  mkdir -p target/fonts
  curl -fsSL -o "$FONT" "$FONT_URL/JuliaMono-Regular.ttf"
  curl -fsSL -o target/fonts/JuliaMono-LICENSE.txt "$FONT_URL/LICENSE"
fi
echo "$FONT_SHA  $FONT" | shasum -a 256 -c - >/dev/null || { echo "font checksum mismatch: $FONT" >&2; exit 1; }
cp "$FONT" target/fonts/JuliaMono-LICENSE.txt "$APP/Contents/Resources/"

VERSION=$(target/dist/makedo --version | cut -d' ' -f2)
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Makedo</string>
  <key>CFBundleIdentifier</key><string>dev.makedo.Makedo</string>
  <key>CFBundleExecutable</key><string>launch</string>
  <key>CFBundleIconFile</key><string>Makedo</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

# Finder запускает с cwd=/ и куцым PATH: спрашиваем папку проекта (или берём аргумент:
# open -a Makedo --args ~/proj) и стартуем через логин-шелл, чтобы агент видел cargo, git, node.
# имя не `Makedo`: ФС macOS не различает регистр, лаунчер затёр бы бинарь `makedo`
cat > "$APP/Contents/MacOS/launch" <<'EOF'
#!/bin/sh
here=$(cd "$(dirname "$0")" && pwd)
dir=${1:-$(osascript -e 'POSIX path of (choose folder with prompt "Project folder for makedo")')} || exit 0
cd "$dir" || exit 1
exec "${SHELL:-/bin/zsh}" -lic 'exec "$0" app' "$here/makedo"
EOF
chmod +x "$APP/Contents/MacOS/launch"

# macOS инвалидирует подпись при копировании бинаря → SIGKILL при запуске; переподписываем ad-hoc
codesign --force --deep -s - "$APP"
echo "built $APP ($(du -sh "$APP" | cut -f1))"
