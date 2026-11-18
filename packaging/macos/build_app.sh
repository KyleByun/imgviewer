#!/bin/sh
# macOS용 ImgView.app 만들기 (Mac에서 실행)
# Finder, Dock, Launchpad에 아이콘이 보이게 하려면 .app 번들이 필요하다.
#
#   sh packaging/macos/build_app.sh
#
# 결과: target/release/ImgView.app  →  응용 프로그램 폴더로 끌어다 놓으면 설치 끝
# 아이콘: assets/icons/imgview.icns (16~1024px, 둥근 사각형 바깥은 투명)
set -eu
cd "$(dirname "$0")/../.."

cargo build --release

APP="target/release/ImgView.app"
VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -n 1)

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/imgview "$APP/Contents/MacOS/imgview"
cp assets/icons/imgview.icns "$APP/Contents/Resources/ImgView.icns"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>ImgView</string>
    <key>CFBundleDisplayName</key>
    <string>ImgView</string>
    <key>CFBundleIdentifier</key>
    <string>com.imgview.app</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleExecutable</key>
    <string>imgview</string>
    <key>CFBundleIconFile</key>
    <string>ImgView</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>LSMinimumSystemVersion</key>
    <string>10.13</string>
</dict>
</plist>
EOF

# Finder가 예전 아이콘을 기억하고 있으면 새로 읽게 한다
touch "$APP"
echo "만들었습니다: $APP"
