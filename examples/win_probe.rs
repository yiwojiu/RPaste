//! 诊断用：主窗口现在能不能收输入。
//!
//! 打印主窗口的 enabled / visible / 前台状态，以及某个屏幕坐标下**最顶层**
//! 的窗口类名——「点按钮没反应」时，先确认那个位置上到底是谁，再怀疑代码。
//!
//! 用法：`win_probe [x y]`，坐标是屏幕物理像素，省略则取主窗口中心。

use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GWL_STYLE, GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowRect,
    IsWindowVisible, WS_DISABLED, WindowFromPoint,
};

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn main() {
    let args: Vec<i32> = std::env::args()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();

    let title: Vec<u16> = "R-Paste\0".encode_utf16().collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() {
        println!("找不到主窗口");
        return;
    }

    let mut rect = windows_sys::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe { GetWindowRect(hwnd, &mut rect) };
    let (cx, cy) = if args.len() >= 2 {
        (args[0], args[1])
    } else {
        ((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2)
    };

    let point = POINT { x: cx, y: cy };
    let top = unsafe { WindowFromPoint(point) };

    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
    println!(
        "主窗口 {hwnd:?} enabled={} visible={} rect={},{} - {},{}",
        style & WS_DISABLED == 0,
        unsafe { IsWindowVisible(hwnd) },
        rect.left,
        rect.top,
        rect.right,
        rect.bottom
    );
    println!(
        "前台={:?}  点({cx},{cy}) 顶层窗口={top:?} class={}",
        unsafe { GetForegroundWindow() },
        class_of(top)
    );

    // 系统菜单（托盘右键弹出的那个）如果还挂着，会以模态方式吞掉所有点击。
    let menu_class: Vec<u16> = "#32768\0".encode_utf16().collect();
    let menu = unsafe { FindWindowW(menu_class.as_ptr(), std::ptr::null()) };
    if !menu.is_null() {
        println!(
            "发现菜单窗口 {menu:?} visible={} class={}",
            unsafe { IsWindowVisible(menu) },
            class_of(menu)
        );
    }
}
