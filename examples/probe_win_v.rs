//! 探测 Win+V 能不能用 `RegisterHotKey` 抢到手。
//!
//! 结论会决定实现路线：
//! - 注册成功 → 说明 Shell 不是用 RegisterHotKey 占着它，还要再用键盘钩子确认
//!   系统弹窗是否被压住；
//! - 注册失败且错误是 1409（已注册）→ 抢占无望，只能上低级键盘钩子或改注册表。
//!
//! 用法：cargo run --release --example probe_win_v

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
};

const VK_V: u32 = 0x56;
const VK_C: u32 = 0x43;
const ID: i32 = 0x7E57;

fn probe(label: &str, mods: u32, vk: u32) {
    let ok = unsafe { RegisterHotKey(std::ptr::null_mut(), ID, mods | MOD_NOREPEAT, vk) };
    if ok != 0 {
        println!("{label:<22} 注册成功");
        unsafe { UnregisterHotKey(std::ptr::null_mut(), ID) };
    } else {
        let err = unsafe { GetLastError() };
        // 1409 = ERROR_HOTKEY_ALREADY_REGISTERED
        println!("{label:<22} 注册失败，GetLastError = {err}");
    }
}

fn main() {
    probe("Alt+V（当前在用）", MOD_ALT, VK_V);
    probe("Win+V（目标）", MOD_WIN, VK_V);
    probe("Win+Shift+V", MOD_WIN | MOD_SHIFT, VK_V);
    probe("Ctrl+Alt+V", MOD_CONTROL | MOD_ALT, VK_V);
    probe("Win+C（对照：已知被占）", MOD_WIN, VK_C);
}
