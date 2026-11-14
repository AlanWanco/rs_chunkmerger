#!/usr/bin/env bash
# Builds rcm.app so macOS launches the GUI as a normal application instead of
# opening Terminal for the raw Mach-O binary.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

target_dir=$(cargo metadata --format-version 1 --no-deps \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
binary=${1:-$target_dir/release/rs_chunkmerger}
app=${2:-dist/rcm.app}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

if [ ! -x "$binary" ]; then
  echo "找不到可执行文件：$binary" >&2
  echo "先运行 cargo build --release，或把二进制路径作为第一个参数传入。" >&2
  exit 1
fi

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 755 "$binary" "$app/Contents/MacOS/rcm"
cp assets/icon.icns "$app/Contents/Resources/rcm.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>rcm</string>
  <key>CFBundleDisplayName</key><string>rcm</string>
  <key>CFBundleIdentifier</key><string>dev.alanwanco.rcm</string>
  <key>CFBundleExecutable</key><string>rcm</string>
  <key>CFBundleIconFile</key><string>rcm.icns</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSRequiresAquaSystemAppearance</key><false/>
</dict>
</plist>
PLIST

# Ad-hoc signature: enough for local use and for Gatekeeper to treat the bundle
# as a single application after quarantine is cleared.
codesign --force --sign - --timestamp=none "$app" >/dev/null 2>&1 || true

echo "$app"
