//! 把 R-Paste 窗口点到前台，并可选地点击窗口内某个相对坐标。
//!
//! 为什么需要它：从后台脚本/CI 唤起窗口时，`SetForegroundWindow` 会因为
//! 调用者进程没有前台权限而静默失败（Windows 的前台锁），于是后续的键盘
//! 模拟全都打不到窗口上。鼠标点击不受这把锁约束——点一下，窗口自然成为
//! 前台，这条路径在任何权限下都成立。
//!
//! 用法：`focus_click [相对x 相对y]`，不带坐标就只把窗口带到前台。

use std::time::Duration;

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, mouse_event,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowRect, SetCursorPos};

fn main() {
    let args: Vec<i32> = std::env::args()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();

    let title: Vec<u16> = "R-Paste\0".encode_utf16().collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() {
        eprintln!("找不到 R-Paste 窗口（是不是没启动？）");
        std::process::exit(1);
    }

    let mut rect: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        eprintln!("GetWindowRect 失败");
        std::process::exit(1);
    }
    println!(
        "窗口 {}x{} @ ({}, {})",
        rect.right - rect.left,
        rect.bottom - rect.top,
        rect.left,
        rect.top
    );

    // 没有坐标就点客户区中央偏上（标题栏下方，一定是内容区）。
    let (dx, dy) = match args.as_slice() {
        [x, y, ..] => (*x, *y),
        _ => (160, 200),
    };
    let x = rect.left + dx;
    let y = rect.top + dy;

    unsafe {
        SetCursorPos(x, y);
        std::thread::sleep(Duration::from_millis(80));
        mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
        std::thread::sleep(Duration::from_millis(40));
        mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
    }
    std::thread::sleep(Duration::from_millis(200));
    println!("已点击 ({x}, {y})");
}
