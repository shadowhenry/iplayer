//! 窗口的平台级开关：**全屏** 与 **窗口置顶**。
//!
//! GPUI 把这两件事藏在 `PlatformWindow` 里（`toggle_fullscreen` 有，但那个
//! trait 对象是 crate 私有的，`Window` 没有透出来），所以这里顺着
//! `raw-window-handle` 拿到 macOS 的 `NSView`，再用 objc2 操作它的 `NSWindow`。
//!
//! 非 macOS 平台一律返回 `false`，调用方据此提示"当前平台不支持"。

#[cfg(target_os = "macos")]
mod imp {
    use gpui_kit::Window;
    use objc2::rc::Retained;
    use objc2_app_kit::{
        NSFloatingWindowLevel, NSNormalWindowLevel, NSView, NSWindow, NSWindowCollectionBehavior,
        NSWindowStyleMask,
    };
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// 从 GPUI 的窗口摸到背后的 `NSWindow`。
    ///
    /// `Window` 自己有一个同名的固有方法（返回 `AnyWindowHandle`），会把 trait
    /// 方法遮住，所以这里必须用完全限定语法把 `raw_window_handle` 那个调出来。
    fn ns_window(window: &Window) -> Option<Retained<NSWindow>> {
        let handle = <Window as HasWindowHandle>::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };
        // SAFETY: 只会在 GPUI 的事件 / 绘制回调里调用，那是主线程；
        // 而且 `window` 这一借用保证了窗口还活着。
        let view = unsafe { &*(appkit.ns_view.as_ptr() as *const NSView) };
        view.window()
    }

    pub fn toggle_fullscreen(window: &Window) -> bool {
        let Some(win) = ns_window(window) else {
            return false;
        };
        // `toggleFullScreen:` 要求窗口声明自己是"全屏主窗口"，
        // GPUI 没设过这一位，不补上它就是一个空操作。
        let behavior =
            win.collectionBehavior() | NSWindowCollectionBehavior::FullScreenPrimary;
        win.setCollectionBehavior(behavior);
        win.toggleFullScreen(None);
        true
    }

    pub fn is_fullscreen(window: &Window) -> bool {
        ns_window(window)
            .map(|w| w.styleMask().contains(NSWindowStyleMask::FullScreen))
            .unwrap_or(false)
    }

    pub fn set_always_on_top(window: &Window, on: bool) -> bool {
        let Some(win) = ns_window(window) else {
            return false;
        };
        win.setLevel(if on {
            NSFloatingWindowLevel
        } else {
            NSNormalWindowLevel
        });
        true
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use gpui_kit::Window;

    pub fn toggle_fullscreen(_window: &Window) -> bool {
        false
    }

    pub fn is_fullscreen(_window: &Window) -> bool {
        false
    }

    pub fn set_always_on_top(_window: &Window, _on: bool) -> bool {
        false
    }
}

pub use imp::{is_fullscreen, set_always_on_top, toggle_fullscreen};
