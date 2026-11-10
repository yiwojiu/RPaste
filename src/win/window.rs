//! 主窗口的显示 / 隐藏 / 定位，以及「粘贴到上一个窗口」。
//!
//! 为什么不用 `egui::ViewportCommand::Visible`：
//! 窗口一旦被隐藏，winit 就不再派发重绘事件，`App::update` 根本不会被调用，
//! 于是写在 `update` 里的「响应热键」逻辑全部失效——按 Alt+V 也没有任何反应。
//! `ctx.request_repaint()` 同样救不回来，隐藏窗口不产生 `WM_PAINT`。
//!
//! 所以显隐、定位、按键注入全部由 Win32 直接控制。

use std::mem::zeroed;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Duration;

use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput,
    VK_CONTROL, VK_V,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, FindWindowW, GWL_EXSTYLE, GetClassNameW, GetCursorPos, GetForegroundWindow,
    GetWindowLongPtrW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, HWND_TOP, IsIconic,
    IsWindowVisible, PostMessageW, SW_HIDE, SW_RESTORE, SW_SHOW, SWP_NOSIZE, SWP_SHOWWINDOW,
    SetForegroundWindow, SetWindowPos, ShowWindow, WM_PAINT, WS_EX_LAYERED,
};

/// eframe 主窗口的标题，`main.rs` 里 `with_title` 设的就是它。
/// listener 用的隐藏窗口标题是 `RPasteClipboardListener`，不会撞。
const MAIN_TITLE: &str = "R-Paste";

/// 窗口重新拿到前台之后，等多久才发粘贴按键。
///
/// 60ms 在记事本、Chrome 这类程序上够用，但钉钉（Qt 5.15）会静默丢掉早到的
/// 按键：它从失活回到激活时，焦点要等事件循环转一圈才还给输入框。手按
/// Ctrl+V 能粘、自动粘贴没反应，差的就是这段时间。
const PASTE_SETTLE: Duration = Duration::from_millis(180);

/// 唤出前的前台窗口。双击粘贴时要还给它。
static PREV_FOREGROUND: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 找主窗口。每次现查而不是缓存句柄——窗口重建后句柄会变。
pub fn main_window() -> Option<HWND> {
    let title = wide(MAIN_TITLE);
    let hwnd = unsafe { FindWindowW(ptr::null(), title.as_ptr()) };
    if hwnd.is_null() { None } else { Some(hwnd) }
}

/// 在光标位置弹出窗口。
///
/// 定位策略：默认出现在光标**右下方**（不遮挡用户正在看的内容），
/// 贴边时翻到左侧或上方，最后统一夹到当前显示器的工作区里
/// （`rcWork` 已经排除了任务栏）。
pub fn show_at_cursor() {
    show_at_cursor_inner(true);
}

/// 从托盘「设置」唤出：位置同样跟光标，但**不改粘贴目标**。
///
/// 从托盘唤出时前台窗口是任务栏/通知区，把它记成「粘贴目标」的话，
/// 之后双击任何一条历史都会试图往任务栏里 `Ctrl+V`。
pub fn show_at_cursor_keep_target() {
    show_at_cursor_inner(false);
}

fn show_at_cursor_inner(remember_target: bool) {
    let Some(hwnd) = main_window() else { return };

    unsafe {
        // 记住现在谁在前台，粘贴时用得上。必须在抢焦点之前取。
        let prev = GetForegroundWindow();
        if remember_target && !prev.is_null() && prev != hwnd {
            PREV_FOREGROUND.store(prev, Ordering::Relaxed);
        }

        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) == 0 {
            // 拿不到光标就退回原来的行为，至少能显示出来
            ShowWindow(hwnd, SW_SHOW);
            take_foreground(hwnd);
            return;
        }

        let mut rect: RECT = zeroed();
        GetWindowRect(hwnd, &mut rect);
        let win_w = rect.right - rect.left;
        let win_h = rect.bottom - rect.top;

        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let work = if GetMonitorInfoW(mon, &mut mi) != 0 {
            mi.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }
        };

        const GAP: i32 = 14;
        let mut x = pt.x + GAP;
        let mut y = pt.y + GAP;

        // 尺寸记忆会让窗口比眼前这块屏幕还大（换过显示器、拔了外接屏都常见），
        // 那种窗口一旦弹出来，右边和下边就永远在人手够不到的地方——还因为
        // 记着尺寸，下次打开照旧。所以在工作区放不下时先收缩，再定位。
        //
        // 收缩只改长边：等比缩放会把用户特意调成的窄高形状压扁。
        let max_w = (work.right - work.left).max(320);
        let max_h = (work.bottom - work.top).max(320);
        let mut new_w = win_w;
        let mut new_h = win_h;
        if new_w > max_w {
            new_w = max_w;
        }
        if new_h > max_h {
            new_h = max_h;
        }
        let resized = new_w != win_w || new_h != win_h;
        let (win_w, win_h) = (new_w, new_h);

        // 右边放不下就翻到光标左侧
        if x + win_w > work.right {
            x = pt.x - win_w - GAP;
        }
        // 下边放不下就翻到光标上方
        if y + win_h > work.bottom {
            y = pt.y - win_h - GAP;
        }

        // 兜底夹到工作区内，避免负坐标或完全出屏
        x = x.clamp(work.left, (work.right - win_w).max(work.left));
        y = y.clamp(work.top, (work.bottom - win_h).max(work.top));

        let flags = if resized {
            SWP_SHOWWINDOW
        } else {
            SWP_NOSIZE | SWP_SHOWWINDOW
        };
        SetWindowPos(hwnd, HWND_TOP, x, y, win_w, win_h, flags);
        take_foreground(hwnd);
    }
}

pub fn show_main_window() {
    let Some(hwnd) = main_window() else { return };
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_SHOW);
        }
    }
    take_foreground(hwnd);
}

pub fn hide_main_window() {
    let Some(hwnd) = main_window() else { return };
    unsafe {
        ShowWindow(hwnd, SW_HIDE);
    }
}

/// 主窗口当前是否可见。
///
/// `SW_HIDE` 只是让窗口不显示，**winit 并不会因此停掉事件循环**：
/// 它仍会一帧帧调用 `App::update`，egui 照样重建整个历史列表，
/// 结果就是窗口藏在托盘里却把一个逻辑核跑满。`update()` 靠这个判断跳过整帧。
pub fn is_main_window_visible() -> bool {
    main_window()
        .map(|hwnd| unsafe { IsWindowVisible(hwnd) != 0 })
        .unwrap_or(false)
}

/// 给隐藏窗口手动补发一个 `WM_PAINT`。
///
/// `SW_HIDE` 之后 Windows 不再产生 `WM_PAINT`，winit 的 `Window::request_redraw()`
/// 对隐藏窗口同样无效，于是 eframe 把 `ControlFlow` 设成 `Poll` 后再也等不到
/// `RedrawRequested` 来改回 `Wait`，主线程就永久空转。补一帧让
/// `run_ui_and_paint` 正常返回 `EventResult::Wait`，循环就能重新睡下去。
pub fn poke_repaint() {
    let Some(hwnd) = main_window() else { return };
    unsafe {
        let _ = PostMessageW(hwnd, WM_PAINT, 0, 0);
    }
}

/// 给主窗口套上系统材质：Win11 的圆角 + 亚克力背景。
///
/// 为什么交给 DWM 而不是在 egui 里画：真正的毛玻璃要模糊「窗口背后的像素」，
/// 而 egui 是即时模式、没有层合成树，拿不到下层内容。系统材质由桌面窗口
/// 管理器合成，效果更准（跟随壁纸、跟随明暗主题），开销也更低。
///
/// 失败不影响功能：Win10 及更早版本不认识这些属性，调用返回错误码，
/// 窗口退回 egui 自己画的半透明底色，只是少了那层材质。
pub fn apply_backdrop() {
    use windows_sys::Win32::Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmSetWindowAttribute};
    use windows_sys::Win32::UI::Controls::MARGINS;

    let Some(hwnd) = main_window() else { return };

    // 直接用数值而不是 `windows_sys` 里的常量名：这两个属性是 Win11 才加的，
    // 老版本 bindings 里没有，写死数字反而不会因为依赖升级而编译失败。
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
    const DWMWCP_ROUND: u32 = 2;
    /// 亚克力（`DWMSBT_TRANSIENTWINDOW`）。这里**不用** Mica：
    /// Mica 是给常驻主窗口的，采样壁纸后基本静止；剪贴板面板是随时弹出的
    /// 临时浮层，需要的是「看得出背后有东西在动」的通透感，那是亚克力的特征。
    const DWMSBT_TRANSIENTWINDOW: u32 = 3;

    unsafe {
        // 诊断：分层窗口（`WS_EX_LAYERED`）会让 DWM 拒绝合成背景材质——
        // 那种窗口的透明由应用自己负责，系统不再模糊它背后的内容。
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        crate::log_info!(
            "窗口扩展样式 {ex:#x}，LAYERED={}",
            (ex & WS_EX_LAYERED) != 0
        );

        // 窗口圆角。客户区是我们自己画的，圆角由 DWM 裁外框。
        let mut corner = DWMWCP_ROUND;
        let hr = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &mut corner as *mut u32 as *const core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if hr < 0 {
            crate::log_info!("圆角属性不可用（{hr:#x}），可能是 Win10");
        }

        let mut backdrop = DWMSBT_TRANSIENTWINDOW;
        let hr = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &mut backdrop as *mut u32 as *const core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if hr < 0 {
            crate::log_info!("系统材质不可用（{hr:#x}），退回纯色背景");
        }

        // 关键一步：让 DWM 把整个客户区纳入「框架扩展」范围。
        //
        // 不做这一步，材质只画在系统框架那几像素上，客户区仍然归应用所有，
        // 于是透明帧缓冲直接把**后面窗口的原始像素**透上来——文字清晰、没有
        // 模糊，比不透明还难看。全 -1 是约定写法，表示扩展到整个客户区。
        let margins = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        let hr = DwmExtendFrameIntoClientArea(hwnd, &margins);
        if hr < 0 {
            crate::log_info!("客户区扩帧失败（{hr:#x}），材质无法铺满");
        }
    }
}

/// 切换可见性，按窗口的**真实**状态判断，不依赖任何自己维护的标志位。
pub fn toggle_main_window() {
    let Some(hwnd) = main_window() else { return };
    let visible = unsafe { IsWindowVisible(hwnd) } != 0;
    if visible {
        hide_main_window();
    } else {
        show_at_cursor();
    }
}

/// 唤出前的前台窗口，就是粘贴的目标。
pub fn previous_foreground() -> Option<HWND> {
    let h = PREV_FOREGROUND.load(Ordering::Relaxed);
    if h.is_null() { None } else { Some(h) }
}

/// 把前台交给 `hwnd`，确认真的切过去了才返回 true。
///
/// 直接 `SetForegroundWindow` 在很多情况下会被系统拒绝（只有前台进程才有资格），
/// 所以这里用 `AttachThreadInput` 把自己的输入队列临时挂到当前前台线程上——
/// 挂上之后系统就认为我们是「同一个输入上下文」，切换才会被接受。
/// 用完必须解绑，否则两个线程的键盘状态会一直互相影响。
fn focus_window(hwnd: HWND) -> bool {
    unsafe {
        if GetForegroundWindow() == hwnd {
            return true;
        }

        let fg = GetForegroundWindow();
        let fg_thread = if fg.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, ptr::null_mut())
        };
        let my_thread = GetCurrentThreadId();

        let attached = fg_thread != 0
            && fg_thread != my_thread
            && AttachThreadInput(my_thread, fg_thread, 1) != 0;

        SetForegroundWindow(hwnd);
        BringWindowToTop(hwnd);

        if attached {
            AttachThreadInput(my_thread, fg_thread, 0);
        }

        for _ in 0..10 {
            if GetForegroundWindow() == hwnd {
                return true;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        false
    }
}

/// 唤出窗口后把前台拿过来；拿不到只留一行日志。
///
/// **不能只调 `SetForegroundWindow`**：本进程不是前台进程时它会被静默拒绝，
/// 窗口照样弹出来，但停在**未激活**状态——一是亚克力材质走未激活配色，
/// 整窗发灰；二是第一下点击会被系统拿去激活窗口，眼前设置里的按钮点不动。
/// 用户看到的就是「怎么灰了，不让设置」。
fn take_foreground(hwnd: HWND) {
    if !focus_window(hwnd) {
        crate::log_warn!("唤出后未能取得前台焦点，窗口会以未激活样式显示（首击被吞）");
    }
}

/// 把内容粘到唤出前的那个窗口。
///
/// **顺序是关键**：必须先 `SetForegroundWindow` 还焦点，再隐藏自己的窗口。
/// 反过来做的话，窗口一隐藏，我们的进程立刻失去前台权限，
/// 之后再调 `SetForegroundWindow` 会被系统直接拒绝（`GetForegroundWindow`
/// 会停在一个既不是目标也不是自己的窗口上），按键就发丢了。
///
/// 返回 false 表示焦点没抢到或按键注入失败。最常见的原因是目标窗口跑在
/// 管理员权限下，跨完整性级别的 `SendInput` 会被 UIPI 静默拦掉。
pub fn paste_to_previous() -> bool {
    let Some(target) = previous_foreground() else {
        return false;
    };
    let started = std::time::Instant::now();

    // 先把焦点还回去。此时我们的进程还握着前台权限，才切得动。
    let _ = focus_window(target);

    // 不管焦点成没成，自己的窗口都得收起来
    hide_main_window();

    // 等前台真的落回目标：hide 之后系统是把前台还给「上一个窗口」，不一定
    // 就是我们记的那个，这里以实际状态为准，不靠 sleep 猜。
    let switched = wait_foreground(target, Duration::from_millis(300));
    if !switched {
        let _ = focus_window(target);
    }

    // 拿到前台 ≠ 能接收字符。钉钉（Qt）这类程序从失活回到激活时，焦点是异步
    // 还给输入框的：按键到得早会被静默丢掉，表现就是「手动 Ctrl+V 能粘、
    // 自动粘贴毫无反应」。这里必须留出缓冲。
    std::thread::sleep(PASTE_SETTLE);

    // 焦点不对就别乱发按键，否则会打到别的窗口上
    let now = unsafe { GetForegroundWindow() };
    if now != target {
        crate::log_warn!(
            "粘贴目标未能取回焦点：{}（当前前台 {}）",
            describe(target),
            describe(now)
        );
        return false;
    }

    let ok = send_ctrl_v();
    // 这条日志是排查「在某些程序里粘不上」的第一手材料：`ok=true` 说明按键
    // 确实投递出去了，那就是对方程序自己忽略了注入的按键（它可能只认扫描码、
    // 或者要求窗口先进入特定状态），而不是我们这边没发。
    crate::log_info!(
        "粘贴 Ctrl+V → {}（{}ms，投递={ok}）",
        describe(target),
        started.elapsed().as_millis()
    );
    ok
}

/// 轮询等待前台变成 `target`。命中返回 true，超时返回 false。
fn wait_foreground(target: HWND, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if unsafe { GetForegroundWindow() } == target {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 给日志用的窗口描述：类名 + 标题。标题可能是空的（某些自绘窗口）。
fn describe(hwnd: HWND) -> String {
    if hwnd.is_null() {
        return "(空)".into();
    }
    unsafe {
        let mut cls = [0u16; 128];
        let n = GetClassNameW(hwnd, cls.as_mut_ptr(), cls.len() as i32);
        let class = String::from_utf16_lossy(&cls[..n.max(0) as usize]);

        let mut title = [0u16; 256];
        let m = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
        let mut text = String::from_utf16_lossy(&title[..m.max(0) as usize]);
        if text.chars().count() > 40 {
            text = text.chars().take(40).collect::<String>() + "…";
        }

        if text.is_empty() {
            format!("[{class}]")
        } else {
            format!("[{class}] {text}")
        }
    }
}

fn key(vk: u16, up: bool) -> INPUT {
    let mut input: INPUT = unsafe { zeroed() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: vk,
        // 扫描码必须填。留 0 时部分程序（winit 系、Chromium 系、以及所有按
        // 扫描码重建按键的输入法/钩子）会把这个事件当成无来源的伪按键丢掉，
        // 表现就是「粘贴到了 A 窗口生效、到 B 窗口毫无反应」。
        wScan: unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16,
        dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
        time: 0,
        dwExtraInfo: 0,
    };
    input
}

fn send_ctrl_v() -> bool {
    // 顺序很重要：Ctrl↓ → V↓ → V↑ → Ctrl↑
    let inputs = [
        key(VK_CONTROL, false),
        key(VK_V, false),
        key(VK_V, true),
        key(VK_CONTROL, true),
    ];

    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    sent == inputs.len() as u32
}
