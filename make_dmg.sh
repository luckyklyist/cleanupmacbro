#!/bin/sh
# Packages "Clean You.app" into a drag-to-Applications disk image:
#   dist/CleanYou-<version>.dmg (and dist/CleanYou.dmg, a copy with a stable name)
# Run ./build_app.sh first (UNIVERSAL=1 for a release).
set -e
cd "$(dirname "$0")"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP="Clean You.app"
VOL="Clean You"
OUT="dist/CleanYou-$VERSION.dmg"
[ -d "$APP" ] || { echo "Missing $APP. Run ./build_app.sh first."; exit 1; }

STAGE=$(mktemp -d)
RW="$STAGE.rw.dmg"
trap 'rm -rf "$STAGE" "$RW"' EXIT
ditto "$APP" "$STAGE/$APP"
ln -s /Applications "$STAGE/Applications"
mkdir "$STAGE/.background"
cp assets/dmg/background.tiff "$STAGE/.background/background.tiff"
cp assets/AppIcon.icns "$STAGE/.VolumeIcon.icns"

# Eject a leftover mount with the same name.
[ -d "/Volumes/$VOL" ] && hdiutil detach "/Volumes/$VOL" -force >/dev/null 2>&1 || true
hdiutil create -quiet -srcfolder "$STAGE" -volname "$VOL" -fs HFS+ -format UDRW -ov "$RW"
DEV=$(hdiutil attach -readwrite -noverify -noautoopen "$RW" | sed -n 's|^\(/dev/disk[0-9]*\).*Apple_HFS.*|\1|p' | head -1)
MNT="/Volumes/$VOL"
SetFile -a C "$MNT" 2>/dev/null || true

# Window layout: 660x420, icons on the arrow drawn in the background.
osascript <<OSA || echo "warning: Finder layout skipped (no GUI session?)"
tell application "Finder"
  tell disk "$VOL"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {200, 120, 860, 562}
    set opts to the icon view options of container window
    set arrangement of opts to not arranged
    set icon size of opts to 112
    set text size of opts to 13
    set background picture of opts to file ".background:background.tiff"
    set position of item "$APP" of container window to {170, 200}
    set position of item "Applications" of container window to {490, 200}
    update without registering applications
    delay 1
    close
  end tell
end tell
OSA

rm -rf "$MNT/.fseventsd"
chmod -Rf go-w "$MNT" 2>/dev/null || true
sync
hdiutil detach "$DEV" -quiet || hdiutil detach "$DEV" -force -quiet
mkdir -p dist
rm -f "$OUT"
hdiutil convert -quiet "$RW" -format UDZO -imagekey zlib-level=9 -o "$OUT"
cp "$OUT" dist/CleanYou.dmg  # stable name for the README download link
echo "Packaged $OUT"
