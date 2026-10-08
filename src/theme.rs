//! 配色。用户明确要求**黑白单色**，不使用蓝色，
//! 所以这里不走主题令牌，直接给出两套调色板。

use gpui_kit::rgb;

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// 窗口底色
    pub bg: u32,
    /// 面板（侧栏 / 控制条）底色
    pub panel: u32,
    /// 舞台底色
    pub stage: u32,
    /// 主文字
    pub text: u32,
    /// 次要文字
    pub muted: u32,
    /// 分隔线
    pub line: u32,
    /// 强调色：浅色下用近黑，深色下用近白
    pub accent: u32,
    /// 悬浮 / 选中底色
    pub hover: u32,
    pub active: u32,
}

pub const LIGHT: Palette = Palette {
    dark: false,
    bg: 0xffffff,
    panel: 0xf6f7f9,
    stage: 0xeceef2,
    text: 0x101216,
    muted: 0x6b7280,
    line: 0xdfe3e8,
    accent: 0x101216,
    hover: 0xe2e6ec,
    active: 0xd0d6de,
};

pub const DARK: Palette = Palette {
    dark: true,
    bg: 0x0d0f12,
    panel: 0x14171b,
    stage: 0x000000,
    text: 0xf4f6f9,
    muted: 0x8b93a1,
    line: 0x262b31,
    accent: 0xf4f6f9,
    hover: 0x262e38,
    active: 0x333c47,
};

impl Palette {
    /// 强调色对应的前景色（保证对比度）。
    pub fn on_accent(&self) -> gpui_kit::Hsla {
        let c = if self.dark { 0x101216 } else { 0xffffff };
        rgb(c).into()
    }

    pub fn bg(&self) -> gpui_kit::Hsla {
        rgb(self.bg).into()
    }
    pub fn panel(&self) -> gpui_kit::Hsla {
        rgb(self.panel).into()
    }
    pub fn stage(&self) -> gpui_kit::Hsla {
        rgb(self.stage).into()
    }
    pub fn text(&self) -> gpui_kit::Hsla {
        rgb(self.text).into()
    }
    pub fn muted(&self) -> gpui_kit::Hsla {
        rgb(self.muted).into()
    }
    pub fn line(&self) -> gpui_kit::Hsla {
        rgb(self.line).into()
    }
    pub fn accent(&self) -> gpui_kit::Hsla {
        rgb(self.accent).into()
    }
    pub fn hover(&self) -> gpui_kit::Hsla {
        rgb(self.hover).into()
    }
    pub fn active(&self) -> gpui_kit::Hsla {
        rgb(self.active).into()
    }
}
