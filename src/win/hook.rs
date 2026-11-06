//! 低级键盘钩子：把 `Win+V` 从系统「剪贴板历史」手里抢过来。
//!
//! 为什么不能用 `RegisterHotKey`：Win+V 被 Shell 独占，实测
//! `RegisterHotKey(MOD_WIN, 'V')` 返回 1450/1409 一律失败，连
//! `Win+Shift+V` 也一样（见 `examples/probe_win_v.rs` 的输出）。
//! 唯一能抢到的方式是在事件到达系统之前把它拦下来。
//!
//! 只吞 V 键，Win 的按下/抬起全程放行——这条是实测出来的，不是猜的
//! （`examples/probe_win_v_hook.rs` 两组对照）：
//!
//! | 做法 | 开始菜单 | 系统剪贴板 | Win 键状态 |
//! |------|----------|------------|------------|
//! | 吞 Win keyup | 不弹 | 不弹 | 0x8000 **残留** |
//! | 放行 Win keyup | 不弹 | 不弹 | 0x0000 干净 |
//!
//! 吞掉 keyup 会让系统认为 Win 一直按着，之后用户按 A 就被当成 Win+A
//! （弹出通知中心），是不可接受的副作用。放行反而什么都不弹——Windows
//! 判定「单独按 Win」比直觉严格。
//!
//! 钩子装在 listener 线程上：那个线程本来就有消息循环（低级钩子的回调
//! 就在装钩子的线程里执行），而且它不做重活。回调里只有原子读和一次
//! `PostMessageW`，绝不阻塞——低级钩子超时会被系统直接摘掉。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_LWIN;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HC_ACTION, KBDLLHOOKSTRUCT, PostMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

/// 钩子命中后投给 listener 窗口的消息。`WM_APP + 2`，和托盘消息错开。
pub const WM_TOGGLE: u32 = 0x8000 + 2;

const VK_RWIN: u32 = 0x5C;
const VK_V: u32 = 0x56;

/// `HHOOK`。用原子 isize 而不是 `OnceLock`，因为要能被 `uninstall` 清回 0。
static HOOK: AtomicIsize = AtomicIsize::new(0);
/// 回调触发时往哪个窗口投消息。
static TARGET: AtomicIsize = AtomicIsize::new(0);
/// Win 键当前是否处于按下状态（左/右任一）。
static WIN_DOWN: AtomicBool = AtomicBool::new(false);
/// 刚吞过一次 Win+V，跟着的 V keyup 也要吞掉，否则系统收到孤立的抬起事件。
static SWALLOW_V_UP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn hook_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: HC_ACTION 时 lparam 一定是有效的 KBDLLHOOKSTRUCT。
        let kb = unsafe { &*(l as *const KBDLLHOOKSTRUCT) };
        let down = w == WM_KEYDOWN as WPARAM || w == WM_SYSKEYDOWN as WPARAM;

        if kb.vkCode == VK_LWIN as u32 || kb.vkCode == VK_RWIN {
            WIN_DOWN.store(down, Ordering::Relaxed);
        } else if kb.vkCode == VK_V {
            if down {
                if WIN_DOWN.load(Ordering::Relaxed) {
                    // 按住不放时系统会连发 keydown，这里必须自己挡：
                    // `SWALLOW_V_UP` 从按下一直挂到抬起，还在挂着就说明
                    // 这是自动重复，只能吞掉、不能再触发一次。
                    // （`Alt+V` 靠 `MOD_NOREPEAT` 由系统挡，钩子没有这个待遇。）
                    if SWALLOW_V_UP.swap(true, Ordering::Relaxed) {
                        return 1;
                    }
                    let hwnd = TARGET.load(Ordering::Relaxed) as HWND;
                    if !hwnd.is_null() {
                        unsafe { PostMessageW(hwnd, WM_TOGGLE, 0, 0) };
                    }
                    return 1; // 吞掉，系统剪贴板历史收不到
                }
            } else if SWALLOW_V_UP.swap(false, Ordering::Relaxed) {
                return 1;
            }
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, w, l) }
}

/// 装钩子。必须在**有消息循环的线程**里调用。
pub fn install(hwnd: HWND) -> Result<(), String> {
    if is_installed() {
        return Ok(());
    }
    TARGET.store(hwnd as isize, Ordering::Relaxed);
    let hook =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), std::ptr::null_mut(), 0) };
    if hook.is_null() {
        TARGET.store(0, Ordering::Relaxed);
        return Err("SetWindowsHookExW(WH_KEYBOARD_LL) 失败".to_owned());
    }
    HOOK.store(hook as isize, Ordering::Relaxed);
    Ok(())
}

/// 卸钩子。退出前必须调，否则系统里留着一个指向已卸载模块的回调。
pub fn uninstall() {
    let h = HOOK.swap(0, Ordering::Relaxed);
    if h != 0 {
        unsafe { UnhookWindowsHookEx(h as _) };
    }
    TARGET.store(0, Ordering::Relaxed);
    WIN_DOWN.store(false, Ordering::Relaxed);
    SWALLOW_V_UP.store(false, Ordering::Relaxed);
}

pub fn is_installed() -> bool {
    HOOK.load(Ordering::Relaxed) != 0
}
