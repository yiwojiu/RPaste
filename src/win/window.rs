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
    INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_CONTROL, VK_V,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, FindWindowW, GetCursorPos, GetForegroundWindow, GetWindowRect,
    GetWindowThreadProcessId, HWND_TOP, IsIconic, IsWindowVisible, PostMessageW, SW_HIDE,
    SW_RESTORE, SW_SHOW, SWP_NOSIZE, SWP_SHOWWINDOW, SetForegroundWindow, SetWindowPos, ShowWindow,
    WM_PAINT,
};

/// eframe 主窗口的标题，`main.rs` 里 `with_title` 设的就是它。
/// listener 用的隐藏窗口标题是 `RPasteClipboardListener`，不会撞。
const MAIN_TITLE: &str = "R-Paste";

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
    let Some(hwnd) = main_window() else { return };

    unsafe {
        // 记住现在谁在前台，粘贴时用得上。必须在抢焦点之前取。
        let prev = GetForegroundWindow();
        if !prev.is_null() && prev != hwnd {
            PREV_FOREGROUND.store(prev, Ordering::Relaxed);
        }

        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) == 0 {
            // 拿不到光标就退回原来的行为，至少能显示出来
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
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

        SetWindowPos(hwnd, HWND_TOP, x, y, 0, 0, SWP_NOSIZE | SWP_SHOWWINDOW);
        SetForegroundWindow(hwnd);
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
        SetForegroundWindow(hwnd);
    }
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

    // 先把焦点还回去。此时我们的进程还握着前台权限，才切得动。
    let _ = focus_window(target);

    // 不管焦点成没成，自己的窗口都得收起来
    hide_main_window();
    std::thread::sleep(Duration::from_millis(60));

    // 焦点不对就别乱发按键，否则会打到别的窗口上
    let now = unsafe { GetForegroundWindow() };
    if now != target && !focus_window(target) {
        return false;
    }

    send_ctrl_v()
}

fn key(vk: u16, up: bool) -> INPUT {
    let mut input: INPUT = unsafe { zeroed() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: vk,
        wScan: 0,
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
