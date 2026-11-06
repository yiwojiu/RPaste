//! 全局热键：`Alt+V` 呼出 / 隐藏窗口。
//!
//! 用 `RegisterHotKey`（系统级热键）而不是低级键盘钩子：
//! 低级钩子会拖慢全系统的键盘输入，还会被安全软件盯上。
//! `MOD_NOREPEAT` 保证按住不放只触发一次，不会连发。
//!
//! 热键注册在 listener 线程的 message-only 窗口上——那个窗口本来就有
//! 独立消息循环，收到 `WM_HOTKEY` 后通过 channel 通知主线程，
//! 完全绕开 eframe/winit 的窗口句柄，不依赖 GUI 框架的底层实现细节。

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
};

const HOTKEY_ID: i32 = 0x52; // 'R' = R-Paste
const VK_V: u32 = 0x56; // 'V' 键

/// 把 `Alt+V` 绑定到 `hwnd`。失败返回错误，通常是因为热键已被别的程序占用。
///
/// # Safety
/// `hwnd` 必须是有效的窗口句柄。这里只把它透传给系统 API，不解引用。
pub unsafe fn register(hwnd: HWND) -> Result<(), String> {
    let ok = unsafe { RegisterHotKey(hwnd, HOTKEY_ID, MOD_ALT | MOD_NOREPEAT, VK_V) };
    if ok == 0 {
        // Alt+V 可能被 Windows 自带「剪贴板历史」或其他工具占用，冲突很常见。
        return Err("Alt+V 注册失败（可能被其他程序占用）".to_owned());
    }
    Ok(())
}

/// # Safety
/// `hwnd` 必须是之前成功注册过热键的窗口句柄。
pub unsafe fn unregister(hwnd: HWND) {
    unsafe {
        UnregisterHotKey(hwnd, HOTKEY_ID);
    }
}
