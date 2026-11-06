//! 验证主窗口显隐控制。这是「点 X 收进托盘后，热键/托盘能不能唤回」的核心。
//!
//! 用法：
//!     cargo run --release --example window_toggle          # 隐藏→显示→隐藏 走一遍
//!     cargo run --release --example window_toggle state    # 只打印当前状态

use rpaste::win::window;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();

    let Some(hwnd) = window::main_window() else {
        eprintln!("找不到 R-Paste 主窗口，程序可能没在运行");
        std::process::exit(1);
    };
    println!("主窗口 HWND = {hwnd:?}");

    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
        let visible = IsWindowVisible(hwnd) != 0;
        println!("当前可见 = {visible}");
        if mode == "state" {
            return;
        }
    }

    // 「托盘唤回」链路：给 listener 窗口发模拟托盘左键，走的是和真实点击
    // 完全相同的 wndproc 分支。连发几次验证 toggle 语义（显示↔隐藏交替）。
    if mode == "tray" {
        let Some(listener) = find_listener_window() else {
            eprintln!("找不到 listener 窗口");
            std::process::exit(1);
        };
        println!("listener HWND = {listener:?}");

        // 先归位到「可见」，后面每一步的期望才确定
        window::show_main_window();
        std::thread::sleep(std::time::Duration::from_millis(250));

        for step in 1..=4 {
            unsafe {
                use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_LBUTTONUP};
                PostMessageW(
                    listener,
                    rpaste::win::tray::TRAY_MSG,
                    0,
                    WM_LBUTTONUP as isize,
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(350));

            let visible = unsafe {
                use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
                IsWindowVisible(hwnd) != 0
            };
            // 起始可见，所以奇数步被切到隐藏，偶数步显示回来
            let expect = step % 2 == 0;
            println!(
                "第 {step} 次点击托盘：可见 = {visible}（期望 {expect}）{}",
                if visible == expect { "✓" } else { "✗" }
            );
        }

        // 收尾恢复到可见，方便继续用
        window::show_main_window();
        return;
    }

    // 验证「跟随光标」：把光标挪到几个刁钻位置，看窗口落点是否合理。
    if mode == "pos" {
        use windows_sys::Win32::Foundation::RECT;

        use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowRect, SetCursorPos};

        let screen_w = 1920;
        let screen_h = 1080;
        // 左上角、屏幕正中、右下角（会触发放不下时的翻转）
        let cases = [
            (100, 100, "左上"),
            (screen_w / 2, screen_h / 2, "正中"),
            (screen_w - 30, screen_h - 30, "右下角"),
            (screen_w - 30, 200, "右边缘"),
            (300, screen_h - 20, "下边缘"),
        ];

        for (cx, cy, label) in cases {
            window::hide_main_window();
            std::thread::sleep(std::time::Duration::from_millis(150));

            unsafe {
                SetCursorPos(cx, cy);
            }
            std::thread::sleep(std::time::Duration::from_millis(80));

            window::show_at_cursor();
            std::thread::sleep(std::time::Duration::from_millis(250));

            let Some(hwnd) = window::main_window() else {
                continue;
            };
            let mut r: RECT = unsafe { std::mem::zeroed() };
            unsafe { GetWindowRect(hwnd, &mut r) };
            let on_screen =
                r.left >= 0 && r.top >= 0 && r.right <= screen_w && r.bottom <= screen_h;
            println!(
                "{label}({cx},{cy}) → 窗口 [{},{} ~ {},{}] 尺寸 {}x{} {}",
                r.left,
                r.top,
                r.right,
                r.bottom,
                r.right - r.left,
                r.bottom - r.top,
                if on_screen {
                    "在屏内 ✓"
                } else {
                    "出屏 ✗"
                }
            );
        }

        window::show_main_window();
        return;
    }

    // 验证「失焦自动收起」：显示窗口，然后把焦点强行切到任务栏，
    // 看 R-Paste 是否自己藏起来。
    if mode == "blur" {
        use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;

        window::show_main_window();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let before = unsafe { IsWindowVisible(hwnd) } != 0;
        println!("唤出后可见 = {before}");

        // 找一个别的窗口当焦点目标。任务栏是常驻的，最省事。
        let tray_class: Vec<u16> = "Shell_TrayWnd\0".encode_utf16().collect();
        let other = unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW(
                tray_class.as_ptr(),
                std::ptr::null(),
            )
        };
        if other.is_null() {
            eprintln!("找不到任务栏窗口");
            std::process::exit(1);
        }

        force_foreground(other);
        println!("已把焦点切到任务栏，等 800ms");
        std::thread::sleep(std::time::Duration::from_millis(800));

        let after = unsafe { IsWindowVisible(hwnd) } != 0;
        println!(
            "失焦后可见 = {after}　{}",
            if !after {
                "✓ 已自动收起"
            } else {
                "✗ 仍然可见"
            }
        );
        return;
    }

    println!("\n--- 隐藏 ---");
    window::hide_main_window();
    report(hwnd);

    std::thread::sleep(std::time::Duration::from_millis(300));

    println!("--- 显示 ---");
    window::show_main_window();
    report(hwnd);
}

fn find_listener_window() -> Option<windows_sys::Win32::Foundation::HWND> {
    use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;
    let name: Vec<u16> = "RPasteClipboardListener\0".encode_utf16().collect();
    let h = unsafe { FindWindowW(std::ptr::null(), name.as_ptr()) };
    if h.is_null() { None } else { Some(h) }
}

/// 强行把前台交给某个窗口。
///
/// 非前台进程直接调 `SetForegroundWindow` 会被系统忽略，得先把输入队列
/// 挂到当前前台线程上（`AttachThreadInput`），切完立刻解绑。
fn force_foreground(hwnd: windows_sys::Win32::Foundation::HWND) {
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
    };

    unsafe {
        let fg = GetForegroundWindow();
        if fg == hwnd {
            return;
        }
        let fg_thread = if fg.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, std::ptr::null_mut())
        };
        let me = GetCurrentThreadId();
        let attached =
            fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, 1) != 0;

        SetForegroundWindow(hwnd);
        BringWindowToTop(hwnd);

        if attached {
            AttachThreadInput(me, fg_thread, 0);
        }
    }
}

fn report(hwnd: windows_sys::Win32::Foundation::HWND) {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    let visible = unsafe { IsWindowVisible(hwnd) } != 0;
    println!("可见 = {visible}");
}
