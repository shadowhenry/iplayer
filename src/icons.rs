//! 内置 SVG 图标集 —— 照搬原版 icons.js 的 Lucide 风格路径（24×24 网格）。
//!
//! JS 时代图标用 `currentColor` 跟随文字颜色；这里在渲染前把 `currentColor`
//! 替换成调色板给的十六进制色，再走 resvg 光栅化，结果按
//! `(名字, 尺寸, 颜色)` 全局缓存 —— 同一个图标一帧里出现多少次也只画一次。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use gpui_kit::{Hsla, RenderImage};
use resvg::{tiny_skia, usvg};

use crate::player::{image_from_straight_rgba, svg};

/// 线性图标（stroke 跟随 currentColor）。
fn stroke(body: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\" fill=\"none\" \
         stroke=\"currentColor\" stroke-width=\"1.75\" stroke-linecap=\"round\" \
         stroke-linejoin=\"round\">{body}</svg>"
    )
}

/// 填充图标（实心，用于播放 / 暂停这类 Controls 字形）。
fn fill(body: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\" \
         fill=\"currentColor\" stroke=\"none\">{body}</svg>"
    )
}

/// 图标源。路径数据与原 Tauri 版 icons.js 逐字一致（补上了 usvg 要求的 xmlns）。
pub fn source(name: &str) -> Option<String> {
    let s = stroke;
    let f = fill;
    Some(match name {
        // 分隔线画在 24 网格的**正中**（x=12）—— 用户明确要求中间那条线居中，
        // 原来的 Lucide 原版是 x=9.5，视觉上明显偏左。
        "panelLeft" => s("<rect x=\"3\" y=\"3\" width=\"18\" height=\"18\" rx=\"2.5\"/><path d=\"M12 3v18\"/>"),
        "pin" => s("<path d=\"M12 17v5\"/><path d=\"M5 17h14v-1.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V6h1a2 2 0 0 0 0-4H8a2 2 0 0 0 0 4h1v4.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24Z\"/>"),
        "sun" => s("<circle cx=\"12\" cy=\"12\" r=\"4\"/><path d=\"M12 2v2\"/><path d=\"M12 20v2\"/><path d=\"m4.9 4.9 1.4 1.4\"/><path d=\"m17.7 17.7 1.4 1.4\"/><path d=\"M2 12h2\"/><path d=\"M20 12h2\"/><path d=\"m6.3 17.7-1.4 1.4\"/><path d=\"m19.1 4.9-1.4 1.4\"/>"),
        "moon" => s("<path d=\"M20 14.5A8.5 8.5 0 0 1 9.5 4a7 7 0 1 0 10.5 10.5\"/>"),
        "folder" => s("<path d=\"M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z\"/>"),
        "film" => s("<rect x=\"3\" y=\"4\" width=\"18\" height=\"16\" rx=\"2.5\"/><path d=\"M7.5 4v16\"/><path d=\"M16.5 4v16\"/><path d=\"M3 9.5h4.5\"/><path d=\"M3 14.5h4.5\"/><path d=\"M16.5 9.5H21\"/><path d=\"M16.5 14.5H21\"/>"),
        "refresh" => s("<path d=\"M21 12a9 9 0 1 1-2.6-6.4L21 8\"/><path d=\"M21 3v5h-5\"/>"),
        "search" => s("<circle cx=\"11\" cy=\"11\" r=\"7\"/><path d=\"m20.5 20.5-4-4\"/>"),
        "play" => f("<path d=\"M7.5 4.6v14.8a1 1 0 0 0 1.53.85l11.2-7.4a1 1 0 0 0 0-1.7L9.03 3.75A1 1 0 0 0 7.5 4.6\"/>"),
        "pause" => f("<rect x=\"6.5\" y=\"4.5\" width=\"4\" height=\"15\" rx=\"1.3\"/><rect x=\"13.5\" y=\"4.5\" width=\"4\" height=\"15\" rx=\"1.3\"/>"),
        "prev" => f("<path d=\"M18.5 4.9v14.2a1 1 0 0 1-1.55.83L8 13.83v5.67a1 1 0 0 1-2 0V4.5a1 1 0 0 1 2 0v5.67l8.95-6.1a1 1 0 0 1 1.55.83\"/>"),
        "next" => f("<path d=\"M5.5 4.9v14.2a1 1 0 0 0 1.55.83L16 13.83v5.67a1 1 0 0 0 2 0V4.5a1 1 0 0 0-2 0v5.67L7.05 4.07a1 1 0 0 0-1.55.83\"/>"),
        "stop" => f("<rect x=\"6\" y=\"6\" width=\"12\" height=\"12\" rx=\"2\"/>"),
        "volume" => s("<path d=\"M11 5 6.5 8.8H3.4v6.4h3.1L11 19z\"/><path d=\"M15.4 8.9a4.6 4.6 0 0 1 0 6.2\"/><path d=\"M18.3 6a8.6 8.6 0 0 1 0 12\"/>"),
        "volumeX" => s("<path d=\"M11 5 6.5 8.8H3.4v6.4h3.1L11 19z\"/><path d=\"m16 9.5 5 5\"/><path d=\"m21 9.5-5 5\"/>"),
        "info" => s("<circle cx=\"12\" cy=\"12\" r=\"9\"/><path d=\"M12 16.5v-5\"/><path d=\"M12 8.2h.01\"/>"),
        // 循环（用户指定）：直接用「画面角度」让出来的环形箭头造型 ——
        // 比原先的 repeat 折线箭头更圆润耐看；单曲循环时中心补一个 "1"
        "loop" => s("<path d=\"M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8\"/><path d=\"M3 3v5h5\"/>"),
        "loop1" => s("<path d=\"M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8\"/><path d=\"M3 3v5h5\"/><path d=\"M11 10.2h1v3.8\"/>"),
        "camera" => s("<path d=\"M14.6 4h-5.2L7.8 6.2H4.3A2.3 2.3 0 0 0 2 8.5v9.2A2.3 2.3 0 0 0 4.3 20h15.4a2.3 2.3 0 0 0 2.3-2.3V8.5A2.3 2.3 0 0 0 19.7 6.2h-3.5z\"/><circle cx=\"12\" cy=\"13\" r=\"3.6\"/>"),
        "toolbox" => s("<rect x=\"2.5\" y=\"8.5\" width=\"19\" height=\"11.5\" rx=\"2.4\"/><path d=\"M9 8.5V6.7a2.2 2.2 0 0 1 2.2-2.2h1.6A2.2 2.2 0 0 1 15 6.7v1.8\"/><path d=\"M2.5 13.2h5.2\"/><path d=\"M16.3 13.2h5.2\"/><path d=\"M10.4 13.2v2.2a1 1 0 0 0 1 1h1.2a1 1 0 0 0 1-1v-2.2\"/>"),
        "gif" => s("<rect x=\"2.5\" y=\"4.5\" width=\"19\" height=\"15\" rx=\"2.5\"/><path d=\"M10.5 10.2a2 2 0 0 0-3.4 1.4v1a2 2 0 0 0 3.4 1.4\"/><path d=\"M13 10v4\"/><path d=\"M16.5 14v-4h2.2\"/><path d=\"M16.5 12.2h1.8\"/>"),
        "expand" => s("<path d=\"M8.5 3H5.5A2.5 2.5 0 0 0 3 5.5v3\"/><path d=\"M15.5 3h3A2.5 2.5 0 0 1 21 5.5v3\"/><path d=\"M15.5 21h3a2.5 2.5 0 0 0 2.5-2.5v-3\"/><path d=\"M8.5 21h-3A2.5 2.5 0 0 1 3 18.5v-3\"/>"),
        "music" => s("<path d=\"M9 18.5V5.2l11-1.9v13.2\"/><circle cx=\"6.2\" cy=\"18.5\" r=\"2.8\"/><circle cx=\"17.2\" cy=\"16.5\" r=\"2.8\"/>"),
        "image" => s("<rect x=\"3\" y=\"3\" width=\"18\" height=\"18\" rx=\"2.5\"/><circle cx=\"8.8\" cy=\"8.8\" r=\"1.9\"/><path d=\"m21 15.5-4.6-4.6L5 21\"/>"),
        "download" => s("<path d=\"M12 3v12\"/><path d=\"m7.2 10.2 4.8 4.8 4.8-4.8\"/><path d=\"M4.5 21h15\"/>"),
        "crop" => s("<path d=\"M6 2.5v13.5a2 2 0 0 0 2 2h13.5\"/><path d=\"M18 21.5V8a2 2 0 0 0-2-2H2.5\"/>"),
        // 画面角度（用户指定的新图标）：一块"画面"（横矩形）+ 内部的
        // 顺时针旋转弧箭头 —— 19px 下箭头仍清晰可辨，且与循环的
        // 环形箭头一眼就能分开
        "rotate" => s("<rect x=\"3.5\" y=\"5\" width=\"17\" height=\"14\" rx=\"2.5\"/><path d=\"M14.6 9.4a3.3 3.3 0 1 0 .9 2.6\"/><path d=\"M14.9 7v2.6h-2.6\"/>"),
        // 字幕（控制条「工具箱」左侧那颗）：一块圆角"画面" + 底部两条短横线
        //（两条线刻意下移、居中，与 gif 的字母造型、image 的几何图案都不同）
        "captions" => s("<rect x=\"2.5\" y=\"4.5\" width=\"19\" height=\"15\" rx=\"2.5\"/><path d=\"M7.5 14h4\"/><path d=\"M13.5 14h3\"/>"),
        // 窗口最小化 / 最大化：一条横线、一个圆角方框（线宽与 X / 减号一致，
        // 方框取 15/24 —— 比满格的 18/24 秀气，和旁边一排字重对得上）
        "minimize" => s("<path d=\"M5 12h14\"/>"),
        "maximize" => s("<rect x=\"4.5\" y=\"4.5\" width=\"15\" height=\"15\" rx=\"2.2\"/>"),
        "close" => s("<path d=\"M18 6 6 18\"/><path d=\"m6 6 12 12\"/>"),
        _ => return None,
    })
}

/// 测试与调试用的全集。
#[cfg(test)]
pub const ALL: &[&str] = &[
    "panelLeft", "pin", "sun", "moon", "folder", "film", "refresh", "search",
    "play", "pause", "prev", "next", "stop", "volume", "volumeX", "info",
    "loop", "loop1", "camera", "toolbox", "gif", "expand", "music",
    "image", "download", "crop", "rotate", "captions", "minimize", "maximize", "close",
];

type Cache = HashMap<(String, u32, String), Arc<RenderImage>>;

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// 把图标栅格化成 GPUI 纹理（正方形）。`hex` 形如 `#e6e8ec`。
pub fn render(name: &str, size: f32, hex: &str) -> Arc<RenderImage> {
    let w = (size.round() as u32).max(1);
    let key = (name.to_string(), w, hex.to_string());
    cache()
        .lock()
        .unwrap()
        .entry(key)
        .or_insert_with(|| render_uncached(name, w, hex))
        .clone()
}

fn render_uncached(name: &str, w: u32, hex: &str) -> Arc<RenderImage> {
    let src = source(name).unwrap_or_else(|| panic!("未知图标: {name}"));
    let src = src.replace("currentColor", hex);
    let tree = usvg::Tree::from_str(&src, &usvg::Options::default())
        .unwrap_or_else(|e| panic!("图标 {name} 解析失败: {e}"));

    let side = tree.size().width().max(1.0);
    let scale = w as f32 / side;
    let mut pixmap = tiny_skia::Pixmap::new(w, w).expect("图标画布");
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    let mut buf = pixmap.take();
    svg::demultiply(&mut buf);
    image_from_straight_rgba(buf, w, w)
}

/// `Hsla` -> `#rrggbb`（图标染色用）。
pub fn hex_of(color: Hsla) -> String {
    let c = color.to_rgb();
    let ch = |v: f32| ((v * 255.0).round().clamp(0.0, 255.0)) as u8;
    format!("#{:02x}{:02x}{:02x}", ch(c.r), ch(c.g), ch(c.b))
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn all_icons_render_and_cache() {
        for name in ALL {
            let a = render(name, 16.0, "#ffffff");
            assert_eq!(a.size(0).width.0, 16, "{name} 尺寸不对");
            assert_eq!(a.size(0).height.0, 16, "{name} 尺寸不对");
            let b = render(name, 16.0, "#ffffff");
            assert!(Arc::ptr_eq(&a, &b), "{name} 缓存未命中");
        }
    }

    /// 循环图标（用户指定）：原来的 repeat 折线箭头整个移除，
    /// 循环用「画面角度」让出来的环形箭头；单曲循环带个 "1"。
    #[test]
    fn loop_uses_the_retired_rotate_glyph() {
        assert!(source("repeat").is_none(), "repeat 图标应当已移除");
        assert!(source("repeat1").is_none(), "repeat1 图标应当已移除");

        let loop_src = source("loop").expect("loop 图标应当存在");
        let rot_src = source("rotate").expect("rotate 图标应当存在");
        assert_ne!(loop_src, rot_src, "循环和画面角度不能共用同一个形状");

        let one_src = source("loop1").expect("loop1 图标应当存在");
        assert!(
            one_src.starts_with(&loop_src) || one_src.contains("v3.8"),
            "单曲循环应当是环形箭头 + 中心 \"1\""
        );
    }

    #[test]
    fn hex_of_converts() {
        assert_eq!(hex_of(Hsla::black()), "#000000");
        assert_eq!(hex_of(Hsla::white()), "#ffffff");
    }

    /// 字幕图标（控制条「工具箱」左侧那颗）：要和同排其他"带框"图标
    /// （画面角度 / GIF / 图片 / 工具箱）一眼分得开。
    #[test]
    fn captions_icon_is_distinct() {
        let cap = source("captions").expect("字幕图标应当存在");
        for other in ["rotate", "gif", "image", "toolbox"] {
            assert_ne!(cap, source(other).expect("对照图标应当存在"), "字幕图标不能和 {other} 共用形状");
        }
        assert!(
            cap.contains("M7.5 14h4") && cap.contains("M13.5 14h3"),
            "字幕图标要有两条靠底的短横线，实际：{cap}"
        );
    }

    /// 侧栏开关图标的竖分隔线必须在 24 网格正中（曾偏左到 9.5，被用户挑出来过）。
    #[test]
    fn sidebar_icon_divider_is_centred() {
        let src = source("panelLeft").expect("panelLeft 应当存在");
        assert!(
            src.contains("M12 3v18"),
            "分隔线应当落在 x=12（正中），实际图标源：{src}"
        );
        assert!(
            !src.contains("M9.5 3v18"),
            "不能再用偏左的 x=9.5 分隔线"
        );
    }
}
