#!/usr/bin/env bash
#
# 把 iPlayer 打成一个可解压即用的 Linux 目录 / 压缩包。
#
#   scripts/bundle-linux.sh            # 产出 dist/iPlayer-linux64/
#   scripts/bundle-linux.sh --tar      # 再打一个 tar.gz（含 .desktop）
#   scripts/bundle-linux.sh --tar --run   # 打完直接启动
#
# 只依赖 cargo 与系统 tar；产物是便携目录，不做系统级安装。
# 需要预先装好 ffmpeg / ffprobe（见 README 快速开发一节）。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

APP_NAME="iPlayer"
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
TARGET="$(rustc -vV | awk '/^host:/{print $2}')"
DIST="$ROOT/dist"
OUT="$DIST/$APP_NAME-linux64"

WANT_TAR=0
WANT_RUN=0
for arg in "$@"; do
  case "$arg" in
    --tar) WANT_TAR=1 ;;
    --run) WANT_RUN=1 ;;
    -h|--help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $arg" >&2; exit 2 ;;
  esac
done

if [ "$(uname -s)" != "Linux" ]; then
  echo "这个脚本只处理 Linux 打包（当前：$(uname -s)）。" >&2
  exit 1
fi

echo "==> 1/4 编译 release（${TARGET}）"
cargo build --release

echo "==> 2/4 摆好便携目录"
rm -rf "$OUT"
mkdir -p "$OUT"

cp target/release/iplayer "$OUT/$APP_NAME"
chmod 755 "$OUT/$APP_NAME"

# ffmpeg / ffprobe 放在主程序同级 —— sidecar 查找顺序的第一站
MISSING=0
for tool in ffmpeg ffprobe; do
  src=""
  for cand in "$ROOT/binaries/$tool-$TARGET" "$ROOT/binaries/$tool"; do
    [ -f "$cand" ] && src="$cand" && break
  done
  if [ -n "$src" ]; then
    cp "$src" "$OUT/$tool"
    chmod 755 "$OUT/$tool"
    echo "    ${tool}  -> ${tool}  ($(du -h "$src" | cut -f1))"
  elif command -v "$tool" >/dev/null 2>&1; then
    echo "    系统已装 ${tool}（$OUT 里不再附带），运行时从 PATH 取"
  else
    MISSING=1
    echo "    !! 找不到 ${tool}，装出来的目录没有播放能力" >&2
  fi
done

# 桌面启动器：图标 + .desktop，解压后 ./install.sh 可挂到应用菜单
mkdir -p "$OUT/share/applications" "$OUT/share/icons/hicolor/256x256/apps"
if [ -f "$ROOT/icons/icon.png" ]; then
  cp "$ROOT/icons/icon.png" "$OUT/share/icons/hicolor/256x256/apps/iplayer.png"
else
  echo "    !! 没有 icons/icon.png，启动器会用默认图标" >&2
fi

cat > "$OUT/share/applications/iplayer.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=iPlayer
Comment=本地媒体播放器
Exec=iplayer %F
Icon=iplayer
Terminal=false
Categories=AudioVideo;Player;
MimeType=video/mp4;video/x-matroska;video/quicktime;video/webm;audio/mpeg;audio/flac;image/png;image/jpeg;image/gif;image/svg+xml;
DESKTOP

cat > "$OUT/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# 把当前目录挂到用户级应用菜单（无需 root）
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HOME/.local/bin"
APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor/256x256/apps"
mkdir -p "$BIN" "$APPS" "$ICONS"
sed "s|^Exec=iplayer|Exec=\"$HERE/iPlayer\"|" "$HERE/share/applications/iplayer.desktop" \
  > "$APPS/iplayer.desktop"
[ -f "$HERE/share/icons/hicolor/256x256/apps/iplayer.png" ] \
  && cp "$HERE/share/icons/hicolor/256x256/apps/iplayer.png" "$ICONS/iplayer.png"
ln -sf "$HERE/iPlayer" "$BIN/iplayer"
update-desktop-database "$APPS" 2>/dev/null || true
echo "已装到应用菜单；命令行可用 iplayer（$BIN 需在 PATH 里）"
INSTALL
chmod 755 "$OUT/install.sh"

echo "==> 3/4 写版本说明"
cat > "$OUT/README.txt" <<TXT
iPlayer $VERSION (linux64, $TARGET)

直接运行：  ./$APP_NAME
装进应用菜单：./install.sh
TXT

echo "==> 4/4 收尾"
if [ "$WANT_TAR" = 1 ]; then
  TAR="$DIST/$APP_NAME-$VERSION-linux64.tar.gz"
  rm -f "$TAR"
  tar -C "$DIST" -czf "$TAR" "$(basename "$OUT")"
  echo "    tar.gz -> $TAR"
fi

echo
echo "完成：${OUT}  ($(du -sh "$OUT" | cut -f1))"
if [ "$MISSING" = 1 ]; then
  echo "注意：缺少 ffmpeg/ffprobe，装出来的目录没有播放能力。" >&2
fi
echo "运行需要 Vulkan 可用的显卡驱动（Mesa 默认自带）。"

if [ "$WANT_RUN" = 1 ]; then
  exec "$OUT/$APP_NAME"
fi
