//! 对指定窗口做一次「注入粘贴」，然后截屏 —— 用来判断某个程序为什么不响应
//! 自动粘贴：到底是注入的按键没被接受，还是焦点 / 时序的问题。
//!
//! 用法：`paste_probe <类名子串> [v|vslow|chars] [输出.png]`
//!
//! - `v`      与生产实现一致：一次 SendInput 发完 Ctrl↓ V↓ V↑ Ctrl↑
//! - `vslow`  同一组按键分四批发，中间隔 30ms
//! - `chars`  不走 Ctrl+V，直接注入两个字符（验证「按键能不能进输入框」）

use std::mem::zeroed;
use std::ptr;

use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SRCCOPY, SelectObject,
};
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC,
    MapVirtualKeyW, SendInput, SetFocus, VK_CONTROL, VK_MENU, VK_V,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, FindWindowW, GUITHREADINFO, GetClassNameW, GetForegroundWindow,
    GetGUIThreadInfo, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, SW_HIDE, SW_SHOW,
    SW_RESTORE, SetForegroundWindow, ShowWindow,
};

const MARK: &str = "RPTEST-1762";

struct Find {
    needle: String,
    found: HWND,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let needle = args
        .next()
        .expect("用法: paste_probe <类名子串> [v|vslow|chars] [out.png]");
    let mode = args.next().unwrap_or_else(|| "v".into());
    let out = args.next().unwrap_or_else(|| "paste_probe.png".into());

    let mut find = Find {
        needle,
        found: ptr::null_mut(),
    };
    unsafe {
        EnumWindows(Some(cb), &mut find as *mut Find as LPARAM);
    }
    let hwnd = find.found;
    if hwnd.is_null() {
        println!("没找到类名包含「{}」的可见窗口", find.needle);
        return;
    }

    unsafe {
        let fg = GetForegroundWindow();
        let fg_thread = if fg.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, ptr::null_mut())
        };
        let my = GetCurrentThreadId();
        let attached =
            fg_thread != 0 && fg_thread != my && AttachThreadInput(my, fg_thread, 1) != 0;
        SetForegroundWindow(hwnd);
        BringWindowToTop(hwnd);
        if attached {
            AttachThreadInput(my, fg_thread, 0);
        }
        std::thread::sleep(std::time::Duration::from_millis(350));

        println!(
            "目标 {hwnd:?} {}，前台 {} -> {}",
            describe(hwnd),
            describe(GetForegroundWindow()),
            describe(hwnd)
        );
    }

    rpaste::win::clipboard::write_text(MARK);
    std::thread::sleep(std::time::Duration::from_millis(200));

    match mode.as_str() {
        "chars" => type_chars("AB"),
        "vslow" => paste_slow(),
        // 完整复现 R-Paste 的粘贴顺序：自己的窗口先占前台 → 让位给目标 →
        // 隐藏自己 → 等到前台确实回到目标 → 发键。用来验证「是不是这一步
        // 让目标程序丢了焦点」。
        "flow" => flow(hwnd),
        // 走真实入口：全局热键唤出 → Alt+1 粘贴第一条
        "e2e" => {
            hotkey(&[VK_CONTROL], 0xC0);
            std::thread::sleep(std::time::Duration::from_millis(1000));
            hotkey(&[VK_MENU], b'1' as u16);
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }
        _ => paste_burst(),
    }

    std::thread::sleep(std::time::Duration::from_millis(450));
    shoot(hwnd, &out);
    println!("已保存 {out}");
}

unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> i32 {
    let find = unsafe { &mut *(lp as *mut Find) };
    if unsafe { IsWindowVisible(hwnd) } == 0 {
        return 1;
    }
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        return 1;
    }
    let class = String::from_utf16_lossy(&buf[..n as usize]);
    if class.contains(&find.needle) {
        find.found = hwnd;
        return 0; // 找到就停
    }
    1
}

fn describe(hwnd: HWND) -> String {
    if hwnd.is_null() {
        return "(空)".into();
    }
    unsafe {
        let mut buf = [0u16; 256];
        let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

/// 与生产实现完全一致：一次性投递四个事件。
fn paste_burst() {
    let inputs = [
        key(VK_CONTROL, false),
        key(VK_V, false),
        key(VK_V, true),
        key(VK_CONTROL, true),
    ];
    let n = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    println!("burst 投递 {n}/4");
}

/// 分四批，中间留 30ms —— 排除「同一时间戳的事件被对方合并或丢弃」。
fn paste_slow() {
    for (vk, up) in [
        (VK_CONTROL, false),
        (VK_V, false),
        (VK_V, true),
        (VK_CONTROL, true),
    ] {
        let one = [key(vk, up)];
        unsafe {
            SendInput(1, one.as_ptr(), std::mem::size_of::<INPUT>() as i32);
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    println!("slow 投递完成");
}

/// 发一个组合键（修饰键按顺序按下，最后按主键，再全部弹起）。
fn hotkey(mods: &[u16], vk: u16) {
    let mut inputs = Vec::with_capacity(mods.len() * 2 + 2);
    for m in mods {
        inputs.push(key(*m, false));
    }
    inputs.push(key(vk, false));
    inputs.push(key(vk, true));
    for m in mods.iter().rev() {
        inputs.push(key(*m, true));
    }
    let n = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    println!("hotkey 投递 {n}/{}", inputs.len());
}

fn type_chars(text: &str) {
    for unit in text.encode_utf16() {
        let pair = [unicode(unit, false), unicode(unit, true)];
        unsafe {
            SendInput(2, pair.as_ptr(), std::mem::size_of::<INPUT>() as i32);
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    println!("已注入字符 {text:?}");
}

/// 把前台切给 `hwnd`。本进程不是前台时 `SetForegroundWindow` 会被系统拒绝，
/// 需要先挂到当前前台线程的输入队列上。
fn focus_to(hwnd: HWND) {
    unsafe {
        let fg = GetForegroundWindow();
        let fg_thread = if fg.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, ptr::null_mut())
        };
        let my = GetCurrentThreadId();
        let attached =
            fg_thread != 0 && fg_thread != my && AttachThreadInput(my, fg_thread, 1) != 0;
        SetForegroundWindow(hwnd);
        BringWindowToTop(hwnd);
        if attached {
            AttachThreadInput(my, fg_thread, 0);
        }
    }
}

/// 打印目标窗口线程的焦点状态。`hwndFocus` 为空说明按键会落到无人接收的地方。
fn dump_focus(tag: &str, hwnd: HWND) {
    unsafe {
        let tid = GetWindowThreadProcessId(hwnd, ptr::null_mut());
        let mut info: GUITHREADINFO = zeroed();
        info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(tid, &mut info) == 0 {
            println!("{tag} GetGUIThreadInfo 失败");
            return;
        }
        println!(
            "{tag} hwndFocus={:?} hwndActive={:?} hwndCapture={:?} flags=0x{:x}",
            info.hwndFocus, info.hwndActive, info.hwndCapture, info.flags
        );
    }
}

/// 复现 R-Paste 的粘贴流程，用来判断「自己抢过前台」是不是丢焦点的原因。
fn flow(target: HWND) {
    let title = wide("R-Paste");
    let main = unsafe { FindWindowW(ptr::null(), title.as_ptr()) };
    if main.is_null() {
        println!("找不到 R-Paste 主窗口（标题应含 R-Paste）");
        return;
    }

    // 1. 模拟热键唤出：窗口可见并占住前台
    unsafe { ShowWindow(main, SW_SHOW) };
    focus_to(main);
    std::thread::sleep(std::time::Duration::from_millis(400));
    dump_focus("唤出后", target);

    // 2. 让位给目标，再隐藏自己 —— R-Paste 的真实顺序
    focus_to(target);
    unsafe { ShowWindow(main, SW_HIDE) };

    // 3. 等前台真的回到目标
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
    while std::time::Instant::now() < deadline {
        if unsafe { GetForegroundWindow() } == target {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    dump_focus("让位后", target);

    // 4a. 先观察：不主动干预，看焦点会不会自己回来、要多久
    let t0 = std::time::Instant::now();
    loop {
        if !focus_hwnd(target).is_null() {
            break;
        }
        if t0.elapsed() > std::time::Duration::from_millis(2000) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    println!("焦点自行恢复耗时 {:?}（0 表示一直没回来）", t0.elapsed());
    dump_focus("等待后", target);

    // 4b. 焦点为空就主动按进去，再等缓冲，然后发键
    unsafe {
        let tid = GetWindowThreadProcessId(target, ptr::null_mut());
        let mut info: GUITHREADINFO = zeroed();
        info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(tid, &mut info) != 0 && info.hwndFocus.is_null() {
            let my = GetCurrentThreadId();
            let att = AttachThreadInput(my, tid, 1) != 0;
            SetFocus(target);
            if att {
                AttachThreadInput(my, tid, 0);
            }
            println!("焦点为空，已 SetFocus");
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(180));
    dump_focus("发键前", target);
    paste_burst();
}

fn focus_hwnd(hwnd: HWND) -> HWND {
    unsafe {
        let tid = GetWindowThreadProcessId(hwnd, ptr::null_mut());
        let mut info: GUITHREADINFO = zeroed();
        info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(tid, &mut info) == 0 {
            return ptr::null_mut();
        }
        info.hwndFocus
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn key(vk: u16, up: bool) -> INPUT {
    let mut input: INPUT = unsafe { zeroed() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: vk,
        wScan: unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16,
        dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
        time: 0,
        dwExtraInfo: 0,
    };
    input
}

fn unicode(unit: u16, up: bool) -> INPUT {
    let mut input: INPUT = unsafe { zeroed() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: 0,
        wScan: unit,
        dwFlags: KEYEVENTF_UNICODE | if up { KEYEVENTF_KEYUP } else { 0 },
        time: 0,
        dwExtraInfo: 0,
    };
    input
}

fn shoot(hwnd: HWND, out: &str) {
    unsafe {
        let mut rect: RECT = zeroed();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return;
        }
        let (x, y) = (rect.left, rect.top);
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        if w <= 0 || h <= 0 {
            return;
        }

        let screen = GetDC(ptr::null_mut());
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp as *mut _);
        if BitBlt(mem, 0, 0, w, h, screen, x, y, SRCCOPY | CAPTUREBLT) == 0 {
            println!("BitBlt 失败");
            return;
        }

        let mut bi: BITMAPINFO = zeroed();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            ..zeroed()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        if GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            buf.as_mut_ptr() as *mut _,
            &mut bi,
            DIB_RGB_COLORS,
        ) == 0
        {
            println!("GetDIBits 失败");
            return;
        }

        SelectObject(mem, old);
        DeleteObject(bmp as *mut _);
        DeleteDC(mem);
        ReleaseDC(ptr::null_mut(), screen);

        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let img = image::RgbaImage::from_raw(w as u32, h as u32, buf).expect("尺寸对不上");
        img.save(out).expect("写文件失败");
    }
}
