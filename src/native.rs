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
    use objc2::runtime::{AnyObject, ClassBuilder, NSObject, Sel};
    use objc2::{ClassType, msg_send, sel};
    use objc2_app_kit::{
        NSApplication, NSFloatingWindowLevel, NSNormalWindowLevel, NSView, NSWindow,
        NSWindowCollectionBehavior, NSWindowStyleMask,
    };
    use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSBundle};
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
    // gpui-pre-macos 不处理这条事件，于是自己挂一个处理器。

    /// FourCC（`aevt` 这种四字符码）打包成 Apple Event 用的 OSType。
    const fn four_cc(b: [u8; 4]) -> u32 {
        u32::from_be_bytes(b)
    }
    /// 事件类 / 事件 ID：`aevt` + `odoc` = "请打开这些文档"。
    const AE_OPEN_DOCS_CLASS: u32 = four_cc(*b"aevt");
    const AE_OPEN_DOCS_ID: u32 = four_cc(*b"odoc");
    /// 事件参数里装文件列表的那个键：`----`（AppleScript 里的 keyDirectObject）。
    const KEY_DIRECT_OBJECT: u32 = four_cc(*b"----");

    /// 收下来的待打开文件。事件在主线程的 Apple Event 分发里到达，
    /// 主循环每帧取一次。
    static OPEN_DOCS: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();

    fn open_docs() -> &'static Mutex<Vec<PathBuf>> {
        OPEN_DOCS.get_or_init(|| Mutex::new(Vec::new()))
    }

    /// Apple Event 处理器本体：从事件里把文件 URL 抠出来排进队列。
    ///
    /// 选择子是 `handleEvent:withReplyEvent:`（AppleEventManager 约定）。
    extern "C-unwind" fn handle_open_docs(
        _this: &AnyObject,
        _cmd: Sel,
        event: &NSAppleEventDescriptor,
        _reply: &NSAppleEventDescriptor,
    ) {
        // `paramDescriptorForKeyword:` 在 objc2-foundation 里被 objc2-core-services
        // 门控着（我们没引那个 crate），直接发消息绕过类型绑定。
        let list: Option<Retained<NSAppleEventDescriptor>> = unsafe {
            msg_send![event, paramDescriptorForKeyword: KEY_DIRECT_OBJECT]
        };
        let Some(list) = list else {
            return;
        };
        let mut got: Vec<PathBuf> = Vec::new();
        // 描述符列表是 1-based
        for i in 1..=list.numberOfItems() {
            let Some(item) = list.descriptorAtIndex(i) else {
                continue;
            };
            // `fileURLValue` 会把 typeFileURL / typeAlias / typeFSRef **都**归一化，
            // 比自己去 data 里抠字节靠谱（老发送方给的是 alias）。
            let Some(url) = item.fileURLValue() else {
                continue;
            };
            let Some(path) = url.path() else {
                continue;
            };
            got.push(PathBuf::from(path.to_string()));
        }
        if !got.is_empty() {
            if let Ok(mut queue) = open_docs().lock() {
                queue.extend(got);
            }
        }
    }

    /// 挂上「打开文档」事件处理器。**必须在应用跑起来之前调**：事件是在
    /// `didFinishLaunching` 之后才到的，但处理器得先挂好，否则第一次双击
    /// 打开的那份文件就丢了。
    pub fn install_open_docs() {
        // 处理器只注册一次；类也只建一次（同一个类名不能重复注册）。
        // 存裸地址而不是 `Retained`：objc 对象不是 `Sync`，而静态量必须是。
        // 这个对象要活到进程结束，所以故意不回收。
        static HANDLER: OnceLock<usize> = OnceLock::new();
        let addr = *HANDLER.get_or_init(|| {
            // AppleEventManager 只认"对象 + 选择子"，没有 C 回调那种用法，
            // 所以现造一个只带这一个方法的类。
            let mut builder = ClassBuilder::new(c"IPlayerOpenDocsHandler", NSObject::class())
                .expect("IPlayerOpenDocsHandler 只会注册一次");
            // SAFETY: 方法签名的编码就是 handler 的实际签名（见其定义）。
            unsafe {
                builder.add_method(
                    sel!(handleEvent:withReplyEvent:),
                    handle_open_docs as extern "C-unwind" fn(_, _, _, _),
                );
            }
            let cls = builder.register();
            let handler: Retained<AnyObject> = unsafe { msg_send![cls, new] };
            Retained::into_raw(handler) as usize
        });

        // SAFETY: 上面存的就是这个对象的地址，且它永不释放。
        let handler: &AnyObject = unsafe { &*(addr as *const AnyObject) };

        // SAFETY: `handler` 上有 `handleEvent:withReplyEvent:` 这个方法，
        // 且注册后它一直活着（存在 HANDLER 里）。
        unsafe {
            let manager = NSAppleEventManager::sharedAppleEventManager();
            let _: () = msg_send![
                &manager,
                setEventHandler: handler,
                andSelector: sel!(handleEvent:withReplyEvent:),
                forEventClass: AE_OPEN_DOCS_CLASS,
                andEventID: AE_OPEN_DOCS_ID,
            ];
        }
    }

    /// 取走一个待打开的文件（主循环每帧问一次）。
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

    /// 别的平台没有 Apple Event 那套"打开文档"，命令行参数就是全部入口。
    pub fn install_open_docs() {}

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
    async_sheet_available, default_handler_bundle_id, hide_window, install_open_docs,
    is_fullscreen, is_window_zoomed, own_bundle_id, set_always_on_top, set_default_role_handler,
    show_window, take_open_doc, toggle_fullscreen,
};
