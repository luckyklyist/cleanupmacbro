#!/bin/sh
# Builds "Clean You.app" next to this script. Drag it into /Applications.
#   ./build_app.sh                 # native build for this Mac
#   UNIVERSAL=1 ./build_app.sh     # Apple Silicon + Intel in one app
#   ZIP=1 ./build_app.sh           # also writes dist/CleanYou-<version>-macOS.zip
set -e
cd "$(dirname "$0")"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP="Clean You.app"

if [ "$UNIVERSAL" = "1" ]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  cargo build --release --target aarch64-apple-darwin
  cargo build --release --target x86_64-apple-darwin
  mkdir -p target/universal
  lipo -create -output target/universal/clean-you \
    target/aarch64-apple-darwin/release/clean-you \
    target/x86_64-apple-darwin/release/clean-you
  BIN=target/universal/clean-you
else
  cargo build --release
  BIN=target/release/clean-you
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
cp "$BIN" "$APP/Contents/MacOS/clean-you"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Clean You</string>
  <key>CFBundleDisplayName</key><string>Clean You</string>
  <key>CFBundleIdentifier</key><string>local.cleanyou</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleExecutable</key><string>clean-you</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
</dict></plist>
PLIST
codesign --force --deep -s - "$APP" 2>/dev/null || true
echo "Built $APP ($VERSION)"

if [ "$ZIP" = "1" ]; then
  mkdir -p dist
  OUT="dist/CleanYou-$VERSION-macOS.zip"
  rm -f "$OUT"
  ditto -c -k --keepParent "$APP" "$OUT"
  echo "Packaged $OUT"
fi
