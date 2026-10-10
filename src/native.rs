//! 窗口的平台级开关：**全屏** / **窗口置顶** / **最大化状态查询** /
//! **接收系统派发的"打开文件"** / **设为默认播放器**。
//!
//! GPUI 把「全屏」藏在 `PlatformWindow` 里（那个 trait 对象是 crate 私有的，
//! `Window` 没有透出来），所以这里顺着 `raw-window-handle` 拿到 macOS 的
//! `NSView`，再用 objc2 操作它的 `NSWindow`。
//!
//! **注意**：改窗口尺寸（最大化 / 还原）**不要**在这里同步调 AppKit ——
//! 见 `toggle_maximize` 的注释：同步调用会把 GPUI 的 resize 通知吞掉，
//! 表现是"窗口变大了但里面的画面还是小的"。这里只做"读状态"和
//! "不影响布局的开关"（全屏 / 置顶 / 藏窗口）。
//!
//! 非 macOS 平台一律返回 `false` / `None`，调用方据此提示"当前平台不支持"。

#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    use gpui_kit::Window;
    use objc2::rc::Retained;
    use objc2_app_kit::{
        NSApplication, NSFloatingWindowLevel, NSNormalWindowLevel, NSView, NSWindow,
        NSWindowCollectionBehavior, NSWindowStyleMask,
    };
    use objc2_foundation::{NSBundle, NSURL};
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

    /// rfd 的「异步」保存面板真的会走 sheet（不阻塞）吗？
    ///
    /// rfd 0.15 的 macOS 后端在 `NSApp` 没在跑、或拿不到父窗口时，会**静默退回
    /// 同步 `runModal`**（`backend/macos/modal_future.rs` 里那句 "I will fallback
    /// to sync dialog for you"）。而 `runModal` 会在主线程里再开一层事件循环 ——
    /// 此时外层 GPUI 的事件分发还没退出，嵌套循环里一有事件（鼠标抬起、帧回调）
    /// 就会重入 GPUI → panic → 撞上 ObjC 边界无法 unwind → 直接 abort。
    /// 用户报的"点截图/转 GIF/提取音频就崩"就是这条回退路。
    ///
    /// 所以弹面板之前先自己确认：父窗口拿得到、`NSApplication` 正在跑。
    /// 两样都满足，rfd 才会老老实实走 sheet。
    pub fn async_sheet_available(window: &Window) -> bool {
        if ns_window(window).is_none() {
            return false;
        }
        let Some(mtm) = objc2::MainThreadMarker::new() else {
            return false;
        };
        NSApplication::sharedApplication(mtm).isRunning()
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

    /// 窗口现在是不是"最大化（zoom）"状态 —— 和绿色交通灯同一个状态位。
    ///
    /// 只读，不碰布局，可以放心在渲染 / 回调里调。按钮的图标按它切换
    ///（放大 ↔ 还原），所以点绿灯最大化之后按钮也会跟着变，不用自己记状态。
    pub fn is_window_zoomed(window: &Window) -> bool {
        ns_window(window)
            .map(|w| w.isZoomed())
            .unwrap_or(false)
    }

    /// 把窗口**藏起来**而不是关掉（红绿灯关闭走这条）。
    ///
    /// 窗口销毁了 App 状态就没了，之后 Dock 点图标也无法"回到主界面"——
    /// 只能重建一个空窗口。orderOut 之后 AppKit 认为应用"没有可见窗口"，
    /// 再点 Dock 图标会触发 `applicationShouldHandleReopen`（GPUI 的
    /// `on_reopen`），把窗口 orderFront 回来即可。
    pub fn hide_window(window: &Window) -> bool {
        let Some(win) = ns_window(window) else {
            return false;
        };
        win.orderOut(None);
        true
    }

    /// 把隐藏的窗口亮回来（Dock 点击 / Dock 菜单「显示主界面」共用）。
    pub fn show_window(window: &Window) -> bool {
        let Some(win) = ns_window(window) else {
            return false;
        };
        // 最小化到 Dock 的窗口 orderFront 是唤不回来的，得先 deminiaturize
        if win.isMiniaturized() {
            win.deminiaturize(None);
        }
        win.orderFront(None);
        win.makeKeyAndOrderFront(None);
        // 从其它应用切回来时也要能抢到前台（activateIgnoringOtherApps 已废弃）
        if let Some(mtm) = objc2::MainThreadMarker::new() {
            #[allow(deprecated)]
            NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        }
        true
    }

    // ── 系统派发的「打开文件」 ─────────────────────────────────────────────
    //
    // 用户在访达里双击、右键"打开方式 → iPlayer"、把文件拖到 Dock 图标上、
    // 或者 `open -a iPlayer.app a.mp4`，macOS 都是给应用发一条 **Apple Event**
    // （事件类 `aevt`、事件 ID `odoc`），**文件路径不会出现在 argv 里** ——
    // 实测 `open -a` 起来的进程 argv 是空的，所以光有 Info.plist 的文档类型声明
    // 是没用的：系统认得这个播放器，双击却什么都不会发生。
    //
    // ⚠️ **别自己往 `NSAppleEventManager` 上挂 aevt/odoc 处理器**（这里踩过）：
    // AppKit 在 `-finishLaunching` 里会给同一对 (class, id) 装上它**自己的**
    // 处理器，把我们抢先注册的那份覆盖掉 —— 而它随后把事件交给应用委托的
    // `application:openURLs:`。所以接法是走 gpui 透出来的那个口子
    //（`main.rs` 里 `application.on_open_urls(...)`，见 [`open_urls`]）。
    //
    // 两件事是分开的：
    //   ① 事件 -> 队列：这里是主线程的 Apple Event 分发，**没有任何 GPUI
    //      上下文**（连 `&mut App` 都拿不到），只负责把路径排进队列；
    //   ② 队列 -> 播放：由 UI 侧取件（`App::pump_open_docs`）。

    /// 收下来的待打开文件。事件在主线程的 Apple Event 分发里到达，
    /// UI 那边每帧取一次（帧循环停着的时候由 `App::watch_open_docs` 去取）。
    static OPEN_DOCS: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();

    fn open_docs() -> &'static Mutex<Vec<PathBuf>> {
        OPEN_DOCS.get_or_init(|| Mutex::new(Vec::new()))
    }

    /// 叫醒 UI 的通道。事件到达那一刻我们只有一堆路径、没法让它重绘，
    /// 所以只"拍一下"；真正取件的是 [`App::watch_open_docs`]。
    ///
    /// 为什么要叫醒：帧循环是**按需**的，没有媒体、没有面板时 `tick` 一进来就
    /// 返回，没人再要下一帧 —— 队列里躺着文件也只是躺着（用户看到的就是
    /// "应用起来了，但不播、左侧列表也是空的"）。
    static OPEN_DOC_WAKE: OnceLock<smol::channel::Sender<()>> = OnceLock::new();

    /// 通道的接收端，交给 `App::watch_open_docs` 常驻等信号。
    pub type OpenDocWake = smol::channel::Receiver<()>;

    /// 接上唤醒通道（在 `run` 之前调一次）。
    pub fn set_open_doc_wake(tx: smol::channel::Sender<()>) {
        let _ = OPEN_DOC_WAKE.set(tx);
    }

    /// 排进队列 + 拍一下 UI。
    fn push_paths(paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        if let Ok(mut queue) = open_docs().lock() {
            queue.extend(paths);
        }
        if let Some(tx) = OPEN_DOC_WAKE.get() {
            // 满了 / 没人接都无所谓：队列才是事实来源，UI 取完会再看一眼
            let _ = tx.try_send(());
        }
    }

    /// 应用委托 `application:openURLs:` 送来的 `file://` URL 列表。
    ///
    /// gpui-pre 透出来的是 `NSURL.absoluteString`，也就是**百分号编码**过的
    /// 字符串（路径里有空格 / 中文都会变成 `%20`、`%E4%B8%AD`），自己抠前缀
    /// 再解码容易出错 —— 交给 Foundation 解回本地路径。
    pub fn open_urls(urls: Vec<String>) {
        let mut got: Vec<PathBuf> = Vec::with_capacity(urls.len());
        for u in urls {
            let s = objc2_foundation::NSString::from_str(&u);
            let Some(url) = NSURL::URLWithString(&s) else {
                continue;
            };
            // 只认本地文件：我们没注册任何 URL scheme；而且 `URLWithString`
            // 对"not-a-url"这种相对字符串也会给出一个 URL，不挡一下就会把
            // 垃圾路径塞进播放列表。
            if !url.isFileURL() {
                continue;
            }
            let Some(path) = url.to_file_path() else {
                continue;
            };
            got.push(path);
        }
        push_paths(got);
    }

    /// 取走一个待打开的文件（UI 每帧 / 每次被拍到时问一次）。
    pub fn take_open_doc() -> Option<PathBuf> {
        let mut queue = open_docs().lock().ok()?;
        if queue.is_empty() {
            None
        } else {
            Some(queue.remove(0))
        }
    }

    // ── 默认播放器 / 打开方式 ─────────────────────────────────────────────

    // LaunchServices 是 CoreServices 的子框架，**不能被单独链接**
    // （ld: cannot link directly with 'LaunchServices'），它的符号由 CoreServices
    // 这份伞形框架转出，所以这里声明 CoreServices。
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        /// 设置某个内容类型的默认处理器。角色位：Viewer=1 / Editor=2 / Shell=4。
        /// 返回 0 表示成功。`CFStringRef` 与 `NSString` 是 toll-free bridged，
        /// 所以直接把手上的 NSString 指针传进去。
        fn LSSetDefaultRoleHandlerForContentType(
            content_type: *const core::ffi::c_void,
            role: u32,
            handler_bundle_id: *const core::ffi::c_void,
        ) -> i32;

        /// 查某个内容类型现在归谁打开。返回 +1 的 CFString（要自己释放），
        /// 查不到返回 NULL。
        fn LSCopyDefaultRoleHandlerForContentType(
            content_type: *const core::ffi::c_void,
            role: u32,
        ) -> *mut core::ffi::c_void;
    }

    const K_LS_ROLES_VIEWER: u32 = 1;
    const K_LS_ROLES_ALL: u32 = 0xFFFF_FFFF;

    /// 把 iPlayer 设为这些内容类型的默认打开方式。
    ///
    /// 这就是访达"显示简介 → 打开方式 → 全部更改"背后调的东西 —— 系统里
    /// 唯一能程序化改默认应用的公开 API。
    ///
    /// 返回成功设置的个数。必须以 `.app` 形式运行：它认的是 bundle identifier，
    /// 直接跑 `target/release/iplayer` 时拿不到有效的 bundle id。
    pub fn set_default_role_handler(utis: &[&str]) -> Result<usize, String> {
        let bundle = NSBundle::mainBundle();
        let Some(id) = bundle.bundleIdentifier() else {
            return Err("请以 iPlayer.app 的形式运行（直接跑命令行二进制拿不到 bundle id）".into());
        };

        let mut ok = 0usize;
        let mut first_err = None;
        for uti in utis {
            let uti = objc2_foundation::NSString::from_str(uti);
            let status = unsafe {
                LSSetDefaultRoleHandlerForContentType(
                    (&*uti as *const objc2_foundation::NSString).cast(),
                    K_LS_ROLES_VIEWER,
                    (&*id as *const objc2_foundation::NSString).cast(),
                )
            };
            // 角色位可以对不上（某些类型我们只声明了 Viewer），退一步用 "全部角色"
            let status = if status == 0 {
                status
            } else {
                unsafe {
                    LSSetDefaultRoleHandlerForContentType(
                        (&*uti as *const objc2_foundation::NSString).cast(),
                        K_LS_ROLES_ALL,
                        (&*id as *const objc2_foundation::NSString).cast(),
                    )
                }
            };
            if status == 0 {
                ok += 1;
            } else if first_err.is_none() {
                first_err = Some(format!("{uti}: OSStatus {status}"));
            }
        }

        if ok == 0 {
            let detail = first_err.unwrap_or_else(|| "未知错误".into());
            return Err(format!(
                "系统还没把 iPlayer 当成可用的播放器（{detail}）——\
                 把应用放到「应用程序」文件夹并打开一次，或跑一次 scripts/bundle-macos.sh"
            ));
        }
        Ok(ok)
    }

    /// 某个内容类型现在归谁打开（bundle id）。查不到 / 拿不到自己的 id 返回 None。
    ///
    /// 用来判断"要不要提示用户去设默认"——已经是默认了就别再啰嗦。
    pub fn default_handler_bundle_id(uti: &str) -> Option<String> {
        let uti = objc2_foundation::NSString::from_str(uti);
        let raw = unsafe {
            LSCopyDefaultRoleHandlerForContentType(
                (&*uti as *const objc2_foundation::NSString).cast(),
                K_LS_ROLES_ALL,
            )
        };
        if raw.is_null() {
            return None;
        }
        // `LSCopy*` 给的是 +1 的引用，包成 Retained 由它负责释放；
        // CFString 与 NSString toll-free bridged，可以直接转。
        let handler = unsafe {
            Retained::from_raw(raw as *mut objc2_foundation::NSString)
        }?;
        Some(handler.to_string())
    }

    /// 本应用自己的 bundle id（不在 .app 里跑时为 None）。
    pub fn own_bundle_id() -> Option<String> {
        NSBundle::mainBundle().bundleIdentifier().map(|s| s.to_string())
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::path::PathBuf;

    use gpui_kit::Window;

    /// 非 macOS 平台没有这套 sheet / runModal 的坑，照常放行。
    pub fn async_sheet_available(_window: &Window) -> bool {
        true
    }

    pub fn toggle_fullscreen(_window: &Window) -> bool {
        false
    }

    pub fn is_fullscreen(_window: &Window) -> bool {
        false
    }

    pub fn set_always_on_top(_window: &Window, _on: bool) -> bool {
        false
    }

    /// 别的平台没有 AppKit 的 zoom 状态位（最大化走各自的窗口管理器）。
    pub fn is_window_zoomed(_window: &Window) -> bool {
        false
    }

    /// 别的平台把要打开的文件放在命令行参数里（`main` 那边已经处理了），
    /// 没有 Apple Event 那套东西，这个口子永远是空的。
    pub fn open_urls(_urls: Vec<String>) {}

    /// 同上：没有事件可等，唤醒通道只是为了让 `App::watch_open_docs`
    /// 的类型在两边一致（它会安安静静睡到进程结束）。
    pub type OpenDocWake = smol::channel::Receiver<()>;

    pub fn set_open_doc_wake(_tx: smol::channel::Sender<()>) {}

    pub fn take_open_doc() -> Option<PathBuf> {
        None
    }

    pub fn set_default_role_handler(_utis: &[&str]) -> Result<usize, String> {
        Err("这个平台还没有实现「设为默认播放器」".into())
    }

    /// 别的平台没有 LaunchServices 那套"默认应用"。
    pub fn default_handler_bundle_id(_uti: &str) -> Option<String> {
        None
    }

    pub fn own_bundle_id() -> Option<String> {
        None
    }

    /// 非 macOS 没有"藏窗口"的必要（窗口关闭即进程结束）。
    pub fn hide_window(_window: &Window) -> bool {
        false
    }

    pub fn show_window(_window: &Window) -> bool {
        false
    }
}

pub use imp::{
    OpenDocWake, async_sheet_available, default_handler_bundle_id, hide_window, is_fullscreen,
    is_window_zoomed, open_urls, own_bundle_id, set_always_on_top, set_default_role_handler,
    set_open_doc_wake, show_window, take_open_doc, toggle_fullscreen,
};
