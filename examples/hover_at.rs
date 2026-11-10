//! 只把鼠标移到主窗口内某个相对坐标，不点击。
//!
//! 用来判断「按钮能不能收到指针」：坐标对的话，hover 样式会出现；不对的话，
//! 画面毫无变化——这比盲点一次再猜「到底点没点中」靠谱。

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowRect, SetCursorPos};

fn main() {
    let args: Vec<i32> = std::env::args()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();
    let (x, y) = (
        args.first().copied().unwrap_or(0),
        args.get(1).copied().unwrap_or(0),
    );

    let title: Vec<u16> = "R-Paste\0".encode_utf16().collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() {
        eprintln!("找不到 R-Paste 窗口");
        return;
    }
    let mut rect: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetWindowRect(hwnd, &mut rect) };
    let sx = rect.left + x;
    let sy = rect.top + y;
    unsafe { SetCursorPos(sx, sy) };
    println!(
        "窗口 {},{} - {},{}，鼠标移到窗口内 ({x},{y}) = 屏幕 ({sx},{sy})",
        rect.left, rect.top, rect.right, rect.bottom
    );
}
