#!/bin/sh
# Builds target/Morlaco.app, so macOS shows the app's icon in the Dock and the
# app switcher. Needs Homebrew with gtk4, libadwaita, pkgconf, openssl@3 and
# librsvg (for rsvg-convert), and Rust. Run it from anywhere:
#
#   contrib/macos/bundle.sh
#
# Then drag target/Morlaco.app into /Applications.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
brew_prefix=$(brew --prefix)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)
icon_svg="$root/data/icons/hicolor/scalable/apps/io.github.jeronimoalbi.Morlaco.svg"
app="$root/target/Morlaco.app"

OPENSSL_DIR=$(brew --prefix openssl@3) cargo build --release --locked -p morlaco \
    --manifest-path "$root/Cargo.toml"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

# The program, behind a small launcher. An app started from the Dock gets
# almost no environment, so the launcher points GTK at Homebrew's data files
# (settings schemas and the like).
cp "$root/target/release/morlaco" "$app/Contents/MacOS/morlaco-bin"
cat > "$app/Contents/MacOS/morlaco" <<LAUNCHER
#!/bin/sh
export XDG_DATA_DIRS="$brew_prefix/share\${XDG_DATA_DIRS:+:\$XDG_DATA_DIRS}"
exec "\$(dirname "\$0")/morlaco-bin" "\$@"
LAUNCHER
chmod +x "$app/Contents/MacOS/morlaco"

# The icon: the SVG at every size an .icns file wants.
iconset=$(mktemp -d)/Morlaco.iconset
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    rsvg-convert -w "$size" -h "$size" "$icon_svg" -o "$iconset/icon_${size}x${size}.png"
    rsvg-convert -w $((size * 2)) -h $((size * 2)) "$icon_svg" \
        -o "$iconset/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/Morlaco.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Morlaco</string>
    <key>CFBundleDisplayName</key><string>Morlaco</string>
    <key>CFBundleIdentifier</key><string>io.github.jeronimoalbi.Morlaco</string>
    <key>CFBundleExecutable</key><string>morlaco</string>
    <key>CFBundleIconFile</key><string>Morlaco</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo "Built $app"
