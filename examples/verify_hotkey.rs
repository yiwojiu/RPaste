//! 验证全局热键 `Alt+V`：真实发一次组合键，看窗口是否唤出、再按一次是否收起。
//!
//! 用法：cargo run --release --example verify_hotkey
//!
//! 用 `SendInput` 发真实的键盘事件，而不是直接调 `toggle_main_window()`——
//! 后者只能证明函数本身没坏，证明不了热键还挂在系统里。

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
};

use rpaste::win;

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

/// 修饰键先下后上，顺序不能反。
fn send_alt_v() {
    let inputs = [
        key(0x12, false), // Alt
        key(0x56, false), // V
        key(0x56, true),
        key(0x12, true),
    ];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

fn visible() -> bool {
    win::is_main_window_visible()
}

fn main() {
    // 归位到隐藏，后面每一步的期望才确定。
    win::hide_main_window();
    std::thread::sleep(std::time::Duration::from_millis(300));
    println!("起始：主窗口可见 = {}", visible());

    send_alt_v();
    std::thread::sleep(std::time::Duration::from_millis(700));
    let first = visible();
    println!(
        "第 1 次 Alt+V：可见 = {first}　{}",
        if first {
            "✓ 已唤出"
        } else {
            "✗ 没反应"
        }
    );

    send_alt_v();
    std::thread::sleep(std::time::Duration::from_millis(700));
    let second = visible();
    println!(
        "第 2 次 Alt+V：可见 = {second}　{}",
        if second {
            "✗ 应该收起"
        } else {
            "✓ 已收起"
        }
    );

    // 收尾：恢复到可见，方便接着看。
    win::show_main_window();
}
