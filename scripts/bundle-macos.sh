#!/usr/bin/env bash
#
# 把 iPlayer 打成一个能双击运行的 macOS 应用。
#
#   scripts/bundle-macos.sh            # 产出 dist/iPlayer.app
#   scripts/bundle-macos.sh --dmg      # 再打一个 DMG 安装包
#   scripts/bundle-macos.sh --zip      # 再打一个 zip（方便传给别人）
#   scripts/bundle-macos.sh --install  # 装到 /Applications（想当默认播放器就装这里）
#   scripts/bundle-macos.sh --dmg --run    # 打完直接打开
#
# 做四件事：编译 release、按 .app 的目录规范摆好文件、写 Info.plist（文档类型由
# `iplayer --doc-types` 生成）、ad-hoc 签名，最后**把这份包注册进 LaunchServices**。
# 没有任何第三方打包工具依赖，只用 Xcode 自带的 codesign / hdiutil。
#
# 注册那一步不是可选的：访达的"打开方式"、双击关联、Dock 拖放全靠 LaunchServices
# 认得这个包。而且必须**在 Info.plist 写完、签名之后**再注册 —— 先注册的话系统
# 记下的是一个还没有文档类型的空壳。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

APP_NAME="iPlayer"
BUNDLE_ID="com.iplayer.app"
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
TARGET="$(rustc -vV | awk '/^host:/{print $2}')"
DIST="$ROOT/dist"
APP="$DIST/$APP_NAME.app"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

WANT_DMG=0
WANT_ZIP=0
WANT_RUN=0
WANT_INSTALL=0
for arg in "$@"; do
  case "$arg" in
    --dmg) WANT_DMG=1 ;;
    --zip) WANT_ZIP=1 ;;
    --run) WANT_RUN=1 ;;
    --install) WANT_INSTALL=1 ;;
    -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $arg" >&2; exit 2 ;;
  esac
done

if [ "$(uname -s)" != "Darwin" ]; then
  echo "这个脚本只处理 macOS 的 .app 打包。" >&2
  exit 1
fi

echo "==> 1/6 编译 release（${TARGET}）"
cargo build --release

echo "==> 2/6 搭建 ${APP_NAME}.app 骨架"
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
  echo "    icon.icns -> Contents/Resources/icon.icns"
else
  echo "    !! 没有 icons/icon.icns，应用会显示成默认图标（跑 scripts/make-icons.py 生成）" >&2
fi

echo "==> 3/6 写 Info.plist（文档类型由 iplayer --doc-types 生成）"
# 扩展名清单只有 media.rs 一份：这里用二进制自己吐出来的 XML，免得 shell 那边
# 再抄一遍几十个扩展名 —— 抄漏一个，那种格式就永远关联不上本应用。
DOC_TYPES="$(target/release/iplayer --doc-types)"
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
    <key>NSPrincipalClass</key>          <string>NSApplication</string>
    <key>LSMinimumSystemVersion</key>    <string>12.0</string>
    <key>LSApplicationCategoryType</key> <string>public.app-category.video</string>
    <key>NSHighResolutionCapable</key>   <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
    <!-- 让访达认得出"这个应用能打开哪些媒体文件"（右键 → 打开方式） -->
$DOC_TYPES
</dict>
</plist>
PLIST

echo "==> 4/6 ad-hoc 签名"
# 先签里面的 sidecar，再签整个包；`--deep` 已废弃，所以手动按顺序来。
for tool in ffmpeg ffprobe; do
  [ -f "$APP/Contents/Resources/$tool" ] || continue
  codesign --force --sign - "$APP/Contents/Resources/$tool" 2>/dev/null \
    || echo "    ${tool} 签名失败（不影响本地运行）" >&2
done
codesign --force --sign - "$APP" 2>/dev/null \
  || echo "    应用签名失败（不影响本地运行）" >&2

# 清掉同一个 bundle id 的历史注册（改过路径、挪进过废纸篓都会留下僵尸条目）。
# 它们会被标成 launch-disabled，系统于是认为"这个 id 没有可用的处理器" ——
# 表现出来就是访达里根本看不到 iPlayer，媒体文件也没法跟它关联。
purge_stale_registrations() {
  [ -x "$LSREGISTER" ] || return 0
  # 注意 `path:` 后面是一长串空格，别用 `index($0, ":")+2` 取 —— 那样取出来带前导空格，
  # 后面跟 keep 比就永远不相等（这个坑踩过一次，等于没清）。
  "$LSREGISTER" -dump 2>/dev/null | awk -v keep="$APP" '
    /^path:/ {
      p = $0
      sub(/^path:[[:space:]]*/, "", p)
      sub(/[[:space:]]*\(0x[0-9a-f]+\)$/, "", p)
    }
    /^identifier:/ { if ($2 == "com.iplayer.app" && p != "" && p != keep) print p }
  ' | sort -u | while IFS= read -r stale; do
    [ -n "$stale" ] || continue
    "$LSREGISTER" -u "$stale" 2>/dev/null || true
    echo "    清掉旧注册: $stale"
  done
}

echo "==> 5/6 注册到 LaunchServices（让系统认得这是媒体播放器）"
purge_stale_registrations
touch "$APP"
"$LSREGISTER" -f "$APP" 2>/dev/null || echo "    注册失败（不影响能打开，但关联会不好用）" >&2
# 立刻回读一遍，确认真注册上了 —— 这一步过了，"打开方式"里就一定看得到
CLAIMED="$("$LSREGISTER" -dump 2>/dev/null | awk -v want="$APP" '
  /^path:/ {
    p = $0
    sub(/^path:[[:space:]]*/, "", p)
    sub(/[[:space:]]*\(0x[0-9a-f]+\)$/, "", p)
  }
  /^claimed UTIs:/ { if (p == want) print substr($0, index($0, ":") + 2) }
' | head -1)"
if [ -n "$CLAIMED" ]; then
  echo "    已声明可打开:$CLAIMED"
else
  echo "    !! 系统里没读到本应用声明的文件类型；把应用挪到 /Applications 后再跑一次本脚本" >&2
fi

echo "==> 6/6 收尾"
if [ "$WANT_INSTALL" = 1 ]; then
  DEST="/Applications/$APP_NAME.app"
  rm -rf "$DEST"
  cp -R "$APP" "$DEST"
  touch "$DEST"
  "$LSREGISTER" -f "$DEST" 2>/dev/null || true
  echo "    已安装: $DEST"
fi

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
if [ "$WANT_INSTALL" = 0 ]; then
  echo "提示：想让它当默认播放器，建议装到「应用程序」里 ——"
  echo "      scripts/bundle-macos.sh --install"
fi
echo "首次打开如果被 Gatekeeper 拦下：右键 -> 打开，或"
echo "  xattr -dr com.apple.quarantine \"${APP}\""

if [ "$WANT_RUN" = 1 ]; then
  open "$APP"
fi
