//! 实验：用低级键盘钩子（WH_KEYBOARD_LL）抢 Win+V，看三件事。
//!
//! 1. 钩子能不能拦到 Win+V（含自己 SendInput 注入的事件）；
//! 2. 吞掉之后系统剪贴板历史还弹不弹；
//! 3. **Win 键的 keyup 该不该一起吞**——这是整个方案唯一的真风险：
//!    - 吞了 → 系统认为 Win 一直按着，之后普通按键会被当成 Win+key；
//!    - 不吞 → 系统看到「Win 按下 + Win 抬起，中间什么都没有」，可能弹出开始菜单。
//!
//! 用法：cargo run --release --example probe_win_v_hook

use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_LWIN,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, FindWindowW, GetMessageW, HC_ACTION, IsWindowVisible,
    KBDLLHOOKSTRUCT, MSG, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_SYSKEYDOWN,
};

const VK_V: u32 = 0x56;

static WIN_DOWN: AtomicBool = AtomicBool::new(false);
static HANDLED: AtomicBool = AtomicBool::new(false);
static SWALLOW_WIN_UP: AtomicBool = AtomicBool::new(false);
static SWALLOW_V_UP: AtomicBool = AtomicBool::new(false);

/// 由环境变量决定吞不吞 Win 的 keyup，方便一次跑两遍对比。
fn swallow_win_up_enabled() -> bool {
    std::env::var_os("KEEP_WIN_UP").is_none()
}

unsafe extern "system" fn hook_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = unsafe { &*(l as *const KBDLLHOOKSTRUCT) };
        let down = w == WM_KEYDOWN as WPARAM || w == WM_SYSKEYDOWN as WPARAM;
        let vk = kb.vkCode;

        if vk == VK_LWIN as u32 {
            if down {
                WIN_DOWN.store(true, Ordering::Relaxed);
            } else {
                WIN_DOWN.store(false, Ordering::Relaxed);
                if SWALLOW_WIN_UP.swap(false, Ordering::Relaxed) && swallow_win_up_enabled() {
                    return 1;
                }
            }
        }

        if vk == VK_V {
            if down && WIN_DOWN.load(Ordering::Relaxed) {
                HANDLED.store(true, Ordering::Relaxed);
                SWALLOW_WIN_UP.store(true, Ordering::Relaxed);
                SWALLOW_V_UP.store(true, Ordering::Relaxed);
                return 1;
            }
            if !down && SWALLOW_V_UP.swap(false, Ordering::Relaxed) {
                return 1;
            }
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, w, l) }
}

fn key(vk: u16, up: bool) -> INPUT {
    let mut input: INPUT = unsafe { std::mem::zeroed() };
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

/// 发一次真实的 Win+V（会被自己的钩子看到）。
fn send_win_v() {
    let inputs = [
        key(0x5B, false), // LWIN down
        key(0x56, false), // V down
        key(0x56, true),  // V up
        key(0x5B, true),  // LWIN up
    ];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

fn window_visible_by_title(title: &str) -> bool {
    let w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe { FindWindowW(std::ptr::null(), w.as_ptr()) };
    if h.is_null() {
        return false;
    }
    unsafe { IsWindowVisible(h) != 0 }
}

fn main() {
    let _tid = std::thread::spawn(|| unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), std::ptr::null_mut(), 0);
        if hook.is_null() {
            println!("装钩子失败");
            std::process::exit(1);
        }
        println!("钩子已装（吞 Win keyup = {}）", swallow_win_up_enabled());

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UnhookWindowsHookEx(hook);
    });

    // 给钩子线程一点时间装好
    std::thread::sleep(std::time::Duration::from_millis(400));

    let start_before = window_visible_by_title("Start");
    let clip_before = window_visible_by_title("Clipboard");

    println!("按 Win+V 前：开始菜单 {start_before} / 剪贴板历史 {clip_before}");
    send_win_v();
    std::thread::sleep(std::time::Duration::from_millis(900));

    let start_after = window_visible_by_title("Start");
    let clip_after = window_visible_by_title("Clipboard");

    println!("钩子拦到 Win+V：{}", HANDLED.load(Ordering::Relaxed));
    println!("按完之后：开始菜单 {start_after} / 剪贴板历史 {clip_after}");

    // 关键副作用：吞掉 Win 的 keyup 之后，系统是不是还认为 Win 按着？
    // 最高位为 1 就说明「残留」——之后用户按 A 会被当成 Win+A。
    let state = unsafe { GetAsyncKeyState(VK_LWIN.into()) };
    println!(
        "Win 键残留状态：{state:#06x} → {}",
        if state as u16 & 0x8000 != 0 {
            "仍按下（有残留，会污染后续按键）"
        } else {
            "已释放（干净）"
        }
    );

    // 收尾：如果系统剪贴板历史被我们放跑了，按 Esc 关掉，别干扰用户
    if clip_after {
        let inputs = [key(0x1B, false), key(0x1B, true)];
        unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            );
        }
    }

    std::process::exit(0);
}
