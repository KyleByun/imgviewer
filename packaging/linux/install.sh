#!/bin/sh
# Linux: 현재 사용자에게 ImgView 설치 (관리자 권한 필요 없음)
#  - 실행 파일:  ~/.local/bin/imgview
#  - 아이콘:     ~/.local/share/icons/hicolor/{256x256,512x512}/apps/imgview.png
#               (assets/icons/imgview_256.png, imgview_512.png — 둥근 사각형 바깥은 투명)
#  - 앱 메뉴:    ~/.local/share/applications/ImgView.desktop
#
#   sh packaging/linux/install.sh
#
# 빌드에 필요한 패키지 (Ubuntu):
#   sudo apt install libgtk-3-dev libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
set -eu
cd "$(dirname "$0")/../.."

cargo build --release

DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN_DIR="$HOME/.local/bin"
ICON_THEME="$DATA_DIR/icons/hicolor"
APP_DIR="$DATA_DIR/applications"

install -D -m 755 target/release/imgview "$BIN_DIR/imgview"
install -D -m 644 assets/icons/imgview_256.png "$ICON_THEME/256x256/apps/imgview.png"
install -D -m 644 assets/icons/imgview_512.png "$ICON_THEME/512x512/apps/imgview.png"

mkdir -p "$APP_DIR"
sed -e "s|^Exec=.*|Exec=$BIN_DIR/imgview %f|" \
    packaging/linux/ImgView.desktop > "$APP_DIR/ImgView.desktop"
chmod 644 "$APP_DIR/ImgView.desktop"

# 아이콘/앱 메뉴 캐시 새로 고침 (도구가 있을 때만)
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f "$ICON_THEME" || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "$APP_DIR" || true
fi

echo "설치했습니다. 앱 메뉴에서 ImgView를 찾을 수 있습니다."
echo "바탕 화면에 두려면: cp \"$APP_DIR/ImgView.desktop\" ~/Desktop/  (GNOME은 아이콘을 우클릭 → '실행 허용')"
