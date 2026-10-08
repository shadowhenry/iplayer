//! SVG 光栅化。
//!
//! `image` crate 只管位图，矢量图得另找人 —— 用 **resvg**（usvg 解析 +
//! tiny-skia 光栅化）。先按目标尺寸缩放再渲染，所以放多大都不会糊。
//!
//! 输出是**非预乘**的 BGRA：tiny-skia 给的是预乘 RGBA，这里先把 alpha 除回去，
//! 再交给通用的 R/B 对调流程，跟 PNG 那条路保持一致。

use std::path::Path;
use std::sync::Arc;

use gpui_kit::RenderImage;
use resvg::{tiny_skia, usvg};

/// 光栅化的目标边长区间。小于下限就放大（矢量图放大是免费的），
/// 大于上限就缩小，免得一张 SVG 吃掉几百 MB 显存。
const MIN_LONG_SIDE: f32 = 512.0;
const MAX_LONG_SIDE: f32 = 2048.0;
/// 放大倍数上限，避免 8×8 的图标被拉到 2048 后纹理全是插值糊。
const MAX_UPSCALE: f32 = 32.0;

pub fn load(path: &Path) -> Result<Arc<RenderImage>, String> {
    let data = std::fs::read(path).map_err(|e| format!("读取 SVG 失败: {e}"))?;

    let opt = usvg::Options {
        // 相对路径（<image href="...">、外部字体）按 SVG 所在目录找
        resources_dir: path.parent().map(|p| p.to_path_buf()),
        ..Default::default()
    };
    let tree = usvg::Tree::from_data(&data, &opt).map_err(|e| format!("解析 SVG 失败: {e}"))?;

    let size = tree.size();
    let (sw, sh) = (size.width(), size.height());
    if !(sw.is_finite() && sh.is_finite()) || sw <= 0.0 || sh <= 0.0 {
        return Err("SVG 没有有效尺寸".to_string());
    }

    let long = sw.max(sh);
    let scale = if long > MAX_LONG_SIDE {
        MAX_LONG_SIDE / long
    } else if long < MIN_LONG_SIDE {
        (MIN_LONG_SIDE / long).min(MAX_UPSCALE)
    } else {
        1.0
    };

    let w = ((sw * scale).round() as u32).max(1);
    let h = ((sh * scale).round() as u32).max(1);

    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or("分配 SVG 画布失败")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    let mut buf = pixmap.take();
    demultiply(&mut buf);
    Ok(super::image_from_straight_rgba(buf, w, h))
}

/// 预乘 RGBA -> 非预乘 RGBA（原地）。
pub(crate) fn demultiply(buf: &mut [u8]) {
    for px in buf.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a < 255 {
            for c in &mut px[0..3] {
                *c = (((*c as u32) * 255) / a).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demultiply_undoes_premultiplication() {
        // 预乘：50% alpha 下的纯红是 (128, 0, 0, 128)
        let mut buf = vec![128, 0, 0, 128, 0, 0, 0, 0];
        demultiply(&mut buf);
        assert_eq!(&buf[0..4], &[255, 0, 0, 128]);
        // 全透明像素保持全 0
        assert_eq!(&buf[4..8], &[0, 0, 0, 0]);
    }

    #[test]
    fn svg_renders_to_a_texture() {
        let dir = std::env::temp_dir().join("iplayer-svg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("square.svg");
        std::fs::write(
            &path,
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="24">
                  <rect width="48" height="24" fill="#ff0000"/>
                </svg>"##,
        )
        .unwrap();

        let image = load(&path).expect("应当渲染成功");
        // 48x24 会被放大到长边 512 -> 512x256
        let size = image.size(0);
        assert_eq!(size.width.0 % 512, 0);
        assert!(size.width.0 >= 512);
        assert_eq!(size.width.0, size.height.0 * 2);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn broken_svg_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join("iplayer-svg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.svg");
        std::fs::write(&path, b"<svg><not-closed").unwrap();
        assert!(load(&path).is_err());
        std::fs::remove_file(&path).ok();
    }
}
