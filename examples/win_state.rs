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
    FindWindowW, GetForegroundWindow, GetWindowRect, IsWindowVisible, PostMessageW, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOZORDER, SetWindowPos, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONUP, WM_RBUTTONUP,
    WindowFromPoint,
};

/// listener 隐藏窗口的标题（见 `src/win/listener.rs`）。
const LISTENER_TITLE: &str = "RPasteClipboardListener";
/// 托盘回调消息，`WM_APP + 1`（见 `src/win/tray.rs`）。
const TRAY_MSG: u32 = 0x8000 + 1;
/// 托盘菜单里「设置…」是第几项（从 1 数）。
const SETTINGS_ITEM: usize = 2;

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

    // `win_state tray-toggle`：模拟左键点托盘图标 → 唤出/收起主窗口。
    // 和热键走的是同一条生产路径（`toggle_main_window` → `show_at_cursor`），
    // 但不依赖组合键注册，脚本里也能用。
    if args.first().is_some_and(|a| a == "tray-toggle") {
        let title: Vec<u16> = LISTENER_TITLE.encode_utf16().chain(Some(0)).collect();
        let listener = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
        if listener.is_null() {
            println!("找不到 listener 窗口（进程没在跑？）");
            return;
        }
        unsafe {
            PostMessageW(listener, TRAY_MSG, 0, WM_LBUTTONUP as isize);
        }
        println!("已发送托盘左键消息");
        return;
    }

    // `win_state tray-settings`：模拟右键托盘图标 → 选「设置…」。
    //
    // 给 listener 窗口发托盘回调消息把菜单弹出来，再往菜单窗口里投一次点击。
    // 全程不用 `SendInput`：那需要前台权限，脚本没有。
    if args.first().is_some_and(|a| a == "tray-settings") {
        let title: Vec<u16> = LISTENER_TITLE.encode_utf16().chain(Some(0)).collect();
        let listener = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
        if listener.is_null() {
            println!("找不到 listener 窗口（进程没在跑？）");
            return;
        }
        unsafe {
            PostMessageW(listener, TRAY_MSG, 0, WM_RBUTTONUP as isize);
        }

        // 等菜单窗口真的出现再发键。菜单窗口的类名是固定的 `#32768`。
        // 菜单没弹出来就发方向键/回车，按键会落到**别的**窗口上——在别人的
        // 对话框里按一个回车，比测试失败难收拾得多。
        let menu_class: Vec<u16> = "#32768\0".encode_utf16().collect();
        let mut menu = std::ptr::null_mut();
        for _ in 0..30 {
            menu = unsafe { FindWindowW(menu_class.as_ptr(), std::ptr::null()) };
            if !menu.is_null() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if menu.is_null() {
            println!("菜单没弹出来（{listener:?} 收到消息但没有 TrackPopupMenu）");
            return;
        }

        // 直接把按键消息投给菜单窗口，而不是 `SendInput`：键盘事件只会进
        // **前台线程**的输入队列，而这个脚本没有前台权限（从外部调
        // `SetForegroundWindow` 基本都会被拒），键会落到别的程序上。
        // 投递到窗口的消息不经过输入队列，和前台是谁无关。
        unsafe {
            for _ in 0..SETTINGS_ITEM {
                PostMessageW(menu, WM_KEYDOWN, 0x28, 0); // VK_DOWN
                PostMessageW(menu, WM_KEYUP, 0x28, 0);
            }
            PostMessageW(menu, WM_KEYDOWN, 0x0D, 0); // VK_RETURN
            PostMessageW(menu, WM_KEYUP, 0x0D, 0);
        }
        std::thread::sleep(std::time::Duration::from_millis(700));

        let main_title: Vec<u16> = "R-Paste\0".encode_utf16().collect();
        let main = unsafe { FindWindowW(std::ptr::null(), main_title.as_ptr()) };
        println!(
            "listener={listener:?} 主窗口={main:?} visible={} foreground={:?}",
            unsafe { IsWindowVisible(main) },
            unsafe { GetForegroundWindow() }
        );
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
