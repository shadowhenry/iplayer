#!/usr/bin/env bash
#
# 把 iPlayer 打成一个能双击运行的 macOS 应用。
#
#   scripts/bundle-macos.sh            # 产出 dist/iPlayer.app
#   scripts/bundle-macos.sh --dmg      # 再打一个 DMG 安装包
#   scripts/bundle-macos.sh --zip      # 再打一个 zip（方便传给别人）
#   scripts/bundle-macos.sh --dmg --run    # 打完直接打开
#
# 做三件事：编译 release、按 .app 的目录规范摆好文件、顺手做一次 ad-hoc 签名。
# 没有任何第三方打包工具依赖，只用 Xcode 自带的 codesign / hdiutil。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

APP_NAME="iPlayer"
BUNDLE_ID="com.iplayer.app"
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
TARGET="$(rustc -vV | awk '/^host:/{print $2}')"
DIST="$ROOT/dist"
APP="$DIST/$APP_NAME.app"

WANT_DMG=0
WANT_ZIP=0
WANT_RUN=0
for arg in "$@"; do
  case "$arg" in
    --dmg) WANT_DMG=1 ;;
    --zip) WANT_ZIP=1 ;;
    --run) WANT_RUN=1 ;;
    -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $arg" >&2; exit 2 ;;
  esac
done

if [ "$(uname -s)" != "Darwin" ]; then
  echo "这个脚本只处理 macOS 的 .app 打包。" >&2
  exit 1
fi

echo "==> 1/5 编译 release（${TARGET}）"
cargo build --release

echo "==> 2/5 搭建 ${APP_NAME}.app 骨架"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/iplayer "$APP/Contents/MacOS/$APP_NAME"
chmod 755 "$APP/Contents/MacOS/$APP_NAME"

# ffmpeg / ffprobe 放进 Contents/Resources —— 正是 sidecar 查找顺序里的第二站
MISSING=0
for tool in ffmpeg ffprobe; do
  src="$ROOT/binaries/$tool-$TARGET"
  [ -f "$src" ] || src="$ROOT/binaries/$tool"
  if [ -f "$src" ]; then
    cp "$src" "$APP/Contents/Resources/$tool"
    chmod 755 "$APP/Contents/Resources/$tool"
    echo "    ${tool}  -> Contents/Resources/${tool}  ($(du -h "$src" | cut -f1))"
  else
    MISSING=1
    echo "    !! 找不到 ${tool}（期望 binaries/${tool}-${TARGET}）—— 装出来的应用打不开视频" >&2
  fi
done

if [ -f "$ROOT/icons/icon.icns" ]; then
  cp "$ROOT/icons/icon.icns" "$APP/Contents/Resources/icon.icns"
else
  echo "    !! 没有 icons/icon.icns，应用会显示成默认图标（跑 scripts/make-icons.py 生成）" >&2
fi

echo "==> 3/5 写 Info.plist"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>              <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>       <string>$APP_NAME</string>
    <key>CFBundleExecutable</key>        <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>        <string>$BUNDLE_ID</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundlePackageType</key>       <string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key>           <string>$VERSION</string>
    <key>CFBundleIconFile</key>          <string>icon</string>
    <key>LSMinimumSystemVersion</key>    <string>12.0</string>
    <key>LSApplicationCategoryType</key> <string>public.app-category.video</string>
    <key>NSHighResolutionCapable</key>   <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
    <!-- 让「用 iPlayer 打开」出现在访达的右键菜单里 -->
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>媒体文件</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>LSItemContentTypes</key>
            <array>
                <string>public.movie</string>
                <string>public.audio</string>
                <string>public.image</string>
                <string>public.svg-image</string>
            </array>
        </dict>
    </array>
</dict>
</plist>
PLIST

echo "==> 4/5 ad-hoc 签名"
# 先签里面的 sidecar，再签整个包；`--deep` 已废弃，所以手动按顺序来。
for tool in ffmpeg ffprobe; do
  [ -f "$APP/Contents/Resources/$tool" ] || continue
  codesign --force --sign - "$APP/Contents/Resources/$tool" 2>/dev/null \
    || echo "    ${tool} 签名失败（不影响本地运行）" >&2
done
codesign --force --sign - "$APP" 2>/dev/null \
  || echo "    应用签名失败（不影响本地运行）" >&2

echo "==> 5/5 收尾"
if [ "$WANT_ZIP" = 1 ]; then
  ZIP="$DIST/$APP_NAME-$VERSION.zip"
  rm -f "$ZIP"
  (cd "$DIST" && ditto -c -k --keepParent "$APP_NAME.app" "$ZIP")
  echo "    zip -> $ZIP"
fi

if [ "$WANT_DMG" = 1 ]; then
  DMG="$DIST/$APP_NAME-$VERSION.dmg"
  rm -f "$DMG"
  rm -rf "$DIST/dmg-stage"
  mkdir -p "$DIST/dmg-stage"
  cp -R "$APP" "$DIST/dmg-stage/"
  ln -s /Applications "$DIST/dmg-stage/Applications"
  hdiutil create -volname "$APP_NAME" -srcfolder "$DIST/dmg-stage" \
    -ov -format UDZO "$DMG" >/dev/null
  rm -rf "$DIST/dmg-stage"
  echo "    dmg -> $DMG"
fi

echo
echo "完成：${APP}  ($(du -sh "$APP" | cut -f1))"
if [ "$MISSING" = 1 ]; then
  echo "注意：缺少 ffmpeg/ffprobe，装出来的应用没有播放能力。" >&2
fi
echo "首次打开如果被 Gatekeeper 拦下：右键 -> 打开，或"
echo "  xattr -dr com.apple.quarantine \"${APP}\""

if [ "$WANT_RUN" = 1 ]; then
  open "$APP"
fi
