//! 窗口状态诊断：可见性、矩形、前台窗口、某个屏幕坐标下命中的窗口。
//!
//! 用来区分「截图看着正常但其实窗口已被收起」和「窗口真的在前台但点击没进去」。
//! 用法：
//! - `win_state [屏幕x 屏幕y]` 打印状态
//! - `win_state hotkey`      发一次 `Alt+V`，让窗口按正常路径唤出并抢到前台
//!
//! 为什么需要 `hotkey` 这一步：窗口失去焦点就会自己收起，而外部进程调
//! `SetForegroundWindow` 会被前台锁拒绝。走系统热键是唯一能让它自己站到
//! 前台、同时光标位置正确的路径。

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetForegroundWindow, GetWindowRect, IsWindowVisible, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOZORDER, SetWindowPos, WindowFromPoint,
};

fn key(vk: u16, up: bool) -> INPUT {
    // 扫描码不能省：winit 用 `lParam` 里的扫描码判断**物理键**，只填虚拟键码
    // 时它会得到 `Unidentified`，于是 egui 侧收不到 `Key` 事件——文字照样
    // 通过 WM_CHAR 进来，看起来像「能打字但快捷键全失效」。
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: vk,
        wScan: scan,
        dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
        time: 0,
        dwExtraInfo: 0,
    };
    input
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().is_some_and(|a| a == "hotkey") {
        // 修饰键先下后上，顺序不能反。
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
        std::thread::sleep(std::time::Duration::from_millis(400));
        return;
    }

    // `win_state sendkey ctrl+alt J`：发任意组合键，用来测「改键之后新组合真的生效」。
    if args.first().is_some_and(|a| a == "sendkey") && args.len() >= 3 {
        let vk = {
            let k = &args[2];
            // 标点必须走 OEM 虚拟键码：`,` 是 0xBC 而不是 ASCII 的 0x2C，
            // 直接按 ASCII 发出去系统根本不认这个键。
            let oem = match k.as_str() {
                "," => Some(0xBCu16),
                "." => Some(0xBE),
                "/" => Some(0xBF),
                "-" => Some(0xBD),
                "=" => Some(0xBB),
                ";" => Some(0xBA),
                "'" => Some(0xDE),
                "`" => Some(0xC0),
                "[" => Some(0xDB),
                "]" => Some(0xDD),
                "\\" => Some(0xDC),
                _ => None,
            };
            // 顺序很关键：单个字母/数字按 ASCII 当虚拟键码（`1` 是 0x31，不是 1），
            // 再试 OEM 标点，最后才当数字解析（用于 F1 之类的显式码）。
            let ascii = k
                .chars()
                .next()
                .filter(|c| c.is_ascii_alphanumeric())
                .map(|c| c.to_ascii_uppercase() as u16);
            match (ascii, oem, k.parse::<u16>()) {
                (Some(v), _, _) => v,
                (None, Some(v), _) => v,
                (None, None, Ok(n)) => n,
                _ => 0x41,
            }
        };
        let mods: Vec<u16> = args[1]
            .split('+')
            .filter_map(|m| match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => Some(0x11u16),
                "alt" => Some(0x12),
                "shift" => Some(0x10),
                _ => None,
            })
            .collect();

        let mut inputs = Vec::new();
        for m in &mods {
            inputs.push(key(*m, false));
        }
        inputs.push(key(vk, false));
        inputs.push(key(vk, true));
        for m in mods.iter().rev() {
            inputs.push(key(*m, true));
        }
        unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
        return;
    }

    let title: Vec<u16> = "R-Paste\0".encode_utf16().collect();
    let hwnd: HWND = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() {
        println!("找不到窗口（进程没在跑？）");
        return;
    }

    // `win_state resize 500 700`：从外部改客户区尺寸，用来验证「尺寸记忆」。
    // 用 SetWindowPos 而不是拖边框：拖边框要求窗口在前台，脚本很难保证。
    if args.first().is_some_and(|a| a == "resize") && args.len() >= 3 {
        let w: i32 = args[1].parse().unwrap_or(500);
        let h: i32 = args[2].parse().unwrap_or(700);
        let old = unsafe { IsWindowVisible(hwnd) };
        unsafe {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                w,
                h,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        println!("已把窗口设为 {w}x{h}（之前 visible={old}）");
        return;
    }

    let mut rect: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetWindowRect(hwnd, &mut rect) };
    println!(
        "hwnd={hwnd:?} visible={} rect={},{} - {},{}",
        unsafe { IsWindowVisible(hwnd) },
        rect.left,
        rect.top,
        rect.right,
        rect.bottom
    );

    let fg = unsafe { GetForegroundWindow() };
    println!("foreground={fg:?} 是不是自己={}", fg == hwnd);

    if args.len() >= 2 {
        let x: i32 = args[0].parse().unwrap_or(0);
        let y: i32 = args[1].parse().unwrap_or(0);
        let p = POINT { x, y };
        let hit = unsafe { WindowFromPoint(p) };
        println!("({x},{y}) 命中窗口={hit:?} 是不是自己={}", hit == hwnd);
    }
}
