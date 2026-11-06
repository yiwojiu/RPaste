//! 验证「窗口在前台时，直接键入就是搜索」。
//!
//! 唤出窗口 → 用 SendInput 发真实按键 → PrintWindow 抓图，看搜索框里有没有字。
//!
//! 用法：cargo run --release --example type_test -- 文件

use std::mem::zeroed;
use std::ptr;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
};
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect};

use rpaste::win::window;

const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

fn key_unicode(unit: u16, up: bool) -> INPUT {
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

/// 用 Unicode 注入直接送字符，绕开键盘布局——中文也能发。
fn type_text(text: &str) -> usize {
    let mut sent = 0;
    for unit in text.encode_utf16() {
        let inputs = [key_unicode(unit, false), key_unicode(unit, true)];
        let n = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        if n as usize == inputs.len() {
            sent += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    sent
}

fn shoot(hwnd: HWND, out: &str) -> bool {
    unsafe {
        let mut rect = zeroed();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return false;
        }
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if w <= 0 || h <= 0 {
            return false;
        }

        let screen = GetDC(ptr::null_mut());
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp);

        let ok = PrintWindow(hwnd, mem, PW_RENDERFULLCONTENT);

        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..zeroed()
        };

        let mut buf = vec![0u8; (w * h * 4) as usize];
        GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            buf.as_mut_ptr() as *mut _,
            &mut bmi,
            DIB_RGB_COLORS,
        );

        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(ptr::null_mut(), screen);

        if ok == 0 {
            return false;
        }

        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
            if px[3] == 0 {
                px[3] = 255;
            }
        }

        let img = image::RgbaImage::from_raw(w as u32, h as u32, buf).expect("像素数不符");
        img.save(out).is_ok()
    }
}

/// 发送普通虚拟键（↓ 这种），不是字符。
fn key_vk(vk: u16, up: bool) -> INPUT {
    let mut input: INPUT = unsafe { zeroed() };
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

fn press_down(times: usize) {
    const VK_DOWN: u16 = 0x28;
    for _ in 0..times {
        let inputs = [key_vk(VK_DOWN, false), key_vk(VK_DOWN, true)];
        unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
}

const VK_CONTROL: u16 = 0x11;
const VK_P: u16 = 0x50;
const VK_DELETE: u16 = 0x2E;
const VK_SPACE: u16 = 0x20;
const VK_MENU: u16 = 0x12;

/// 发一个单独的虚拟键。
fn press_key(vk: u16) {
    let inputs = [key_vk(vk, false), key_vk(vk, true)];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(80));
}

/// 发「修饰键 + 键」的组合。修饰键先下后上，顺序不能反。
fn press_combo(modifier: u16, vk: u16) {
    let inputs = [
        key_vk(modifier, false),
        key_vk(vk, false),
        key_vk(vk, true),
        key_vk(modifier, true),
    ];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(120));
}

/// 在屏幕坐标处点一下左键。
fn click_at(x: i32, y: i32) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos;

    unsafe {
        SetCursorPos(x, y);
        std::thread::sleep(std::time::Duration::from_millis(120));

        let mut down: INPUT = zeroed();
        down.r#type = INPUT_MOUSE;
        down.Anonymous.mi = MOUSEINPUT {
            dx: 0,
            dy: 0,
            mouseData: 0,
            dwFlags: MOUSEEVENTF_LEFTDOWN,
            time: 0,
            dwExtraInfo: 0,
        };
        let mut up: INPUT = zeroed();
        up.r#type = INPUT_MOUSE;
        up.Anonymous.mi = MOUSEINPUT {
            dx: 0,
            dy: 0,
            mouseData: 0,
            dwFlags: MOUSEEVENTF_LEFTUP,
            time: 0,
            dwExtraInfo: 0,
        };
        let events = [down, up];
        SendInput(
            events.len() as u32,
            events.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
}

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next().unwrap_or_else(|| "文件".to_string());

    let Some(hwnd) = window::main_window() else {
        eprintln!("R-Paste 没在运行");
        std::process::exit(1);
    };

    // 唤出窗口，内部会把焦点交给搜索框。
    // 等久一点：焦点是异步落定的（Win32 激活 + egui 下一帧才把焦点给输入框），
    // 太急会丢掉第一个字符，看起来像搜索框没响应。
    window::show_at_cursor();
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // 确认前台真的是我们，否则按键会打到别的程序上
    let fg = unsafe { GetForegroundWindow() };
    if fg != hwnd {
        eprintln!("R-Paste 没拿到前台，跳过输入以免打到别的窗口");
        std::process::exit(1);
    }

    if first == "--down" {
        let n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);

        // 按键前先拍一张，和按键后对比才能确定列表到底滚没滚
        if shoot(hwnd, "scroll_before.png") {
            println!("已抓图 scroll_before.png");
        }

        println!("窗口已在前台，按 {n} 次 ↓");
        press_down(n);
        std::thread::sleep(std::time::Duration::from_millis(500));

        // 发键过程中可能因为失焦被自动收起，截图前确认一下还可见。
        // 隐藏状态下 PrintWindow 只能抓到黑图。
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
            if IsWindowVisible(hwnd) == 0 {
                println!("窗口已自动收起，重新显示后再截图");
                window::show_main_window();
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }

        if shoot(hwnd, "scroll_after.png") {
            println!("已抓图 scroll_after.png");
        }
        return;
    }

    // --clear：清空搜索框，让后续测试从干净列表开始
    if first == "--clear" {
        press_combo(VK_CONTROL, 0x41); // Ctrl+A 全选
        press_key(0x08); // Backspace
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = shoot(hwnd, "cleared.png");
        println!("已清空搜索框");
        return;
    }

    // --guard <词>：只输入搜索词，不选中任何行，然后按 Delete。
    // 没有选中项时 Delete 必须留给输入框，历史一条都不能少。
    if first == "--guard" {
        let text = args.next().unwrap_or_else(|| "PIN".to_string());
        type_text(&text);
        std::thread::sleep(std::time::Duration::from_millis(300));
        println!("搜索框输入 {text:?}（未选中任何行）后按 Delete");
        press_key(VK_DELETE);
        std::thread::sleep(std::time::Duration::from_millis(400));

        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
            if IsWindowVisible(hwnd) == 0 {
                window::show_main_window();
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
        let _ = shoot(hwnd, "guard_after.png");
        return;
    }

    // --pin <n>：选中第 n 条后 Ctrl+P 收藏
    // --del <n>：选中第 n 条后按 Delete 删除（先把焦点从搜索框移开）
    if first == "--pin" || first == "--del" {
        let n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);

        let _ = shoot(hwnd, "edit_before.png");
        press_down(n);
        std::thread::sleep(std::time::Duration::from_millis(200));

        if first == "--pin" {
            println!("Ctrl+P 收藏第 {n} 条");
            press_combo(VK_CONTROL, VK_P);
        } else {
            println!("Delete 删除第 {n} 条");
            press_key(VK_DELETE);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));

        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
            if IsWindowVisible(hwnd) == 0 {
                window::show_main_window();
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
        let _ = shoot(hwnd, "edit_after.png");
        return;
    }

    // --ctrl <n>：按 Ctrl+n 切类型筛选（0=全部 1=文本 2=图片 3=文件）
    if first == "--ctrl" {
        let n: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);
        println!("Ctrl+{n} 切类型筛选");
        press_combo(VK_CONTROL, 0x30 + n as u16);
        std::thread::sleep(std::time::Duration::from_millis(500));

        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
            if IsWindowVisible(hwnd) == 0 {
                window::show_main_window();
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
        let _ = shoot(hwnd, "filter_after.png");
        return;
    }

    // --space：切到图片筛选 → 选中第 n 条 → 空格看大图。
    // 必须一个进程内跑完：选中项是 UI 进程内存里的状态，跨进程不共享。
    if first == "--space" {
        let n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
        if n > 0 {
            // Ctrl+0 先回「全部」：Ctrl+数字是切换语义，已经是图片档时再按
            // Ctrl+2 会取消筛选，脚本就不确定了。
            press_combo(VK_CONTROL, 0x30);
            std::thread::sleep(std::time::Duration::from_millis(250));
            press_combo(VK_CONTROL, 0x32); // Ctrl+2 只看图片
            std::thread::sleep(std::time::Duration::from_millis(350));
            press_down(n);
            std::thread::sleep(std::time::Duration::from_millis(300));
        }

        println!("空格打开大图预览");
        press_key(VK_SPACE);
        // 1024px 解码 + 上传纹理要几十毫秒，留足时间再抓图。
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let _ = shoot(hwnd, "preview_open.png");
        return;
    }

    // --alt <n>：Alt+n 直接粘贴第 n 条。
    // 成功后窗口会自动收起（worker 把焦点还回去之后才 hide），所以这里
    // 用「窗口是否隐藏」当作动作生效的证据，再重新唤出来抓图。
    if first == "--alt" {
        let n: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
        println!("Alt+{n} 直接粘贴第 {n} 条");
        press_combo(VK_MENU, 0x30 + n);
        std::thread::sleep(std::time::Duration::from_millis(900));

        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
            let hidden = IsWindowVisible(hwnd) == 0;
            println!("窗口已收起 = {hidden}（true 说明粘贴动作生效）");
            if hidden {
                window::show_main_window();
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
        let _ = shoot(hwnd, "alt_after.png");
        return;
    }

    // --esc：按一次 Esc。用来验证「预览开着时 Esc 只关浮层、不收窗口」。
    // --click-rel <x> <y>：按窗口相对坐标点一下，抓图看效果。
    // 用来验证画在控件内部、没有独立可测入口的小按钮（比如搜索框的清除键）。
    if first == "--click-rel" {
        let x: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let y: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);

        let mut rect = unsafe { zeroed() };
        unsafe { GetWindowRect(hwnd, &mut rect) };
        println!(
            "点击窗口内 ({x},{y}) → 屏幕 ({},{})",
            rect.left + x,
            rect.top + y
        );
        click_at(rect.left + x, rect.top + y);
        std::thread::sleep(std::time::Duration::from_millis(600));
        let _ = shoot(hwnd, "click_rel.png");
        println!("已抓图 click_rel.png");
        return;
    }

    // --alt-enter：Alt+Enter 开预览浮层（搜索词非空时也能开）。
    if first == "--alt-enter" {
        let n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        press_down(n);
        std::thread::sleep(std::time::Duration::from_millis(200));
        press_combo(VK_MENU, 0x0D); // Alt+Enter
        std::thread::sleep(std::time::Duration::from_millis(600));
        let _ = shoot(hwnd, "alt_enter.png");
        println!("Alt+Enter 打开预览");
        return;
    }

    if first == "--esc" {
        println!("按 Esc");
        press_key(0x1B);
        std::thread::sleep(std::time::Duration::from_millis(500));
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
            println!("窗口仍可见 = {}", IsWindowVisible(hwnd) != 0);
        }
        let _ = shoot(hwnd, "esc_after.png");
        return;
    }

    // --click：打开预览后点一下图片中间，验证「点击关闭」。
    if first == "--click" {
        press_combo(VK_CONTROL, 0x30);
        std::thread::sleep(std::time::Duration::from_millis(250));
        press_combo(VK_CONTROL, 0x32);
        std::thread::sleep(std::time::Duration::from_millis(350));
        press_down(1);
        std::thread::sleep(std::time::Duration::from_millis(300));
        press_key(VK_SPACE);
        std::thread::sleep(std::time::Duration::from_millis(1200));

        let mut rect = unsafe { zeroed() };
        unsafe { GetWindowRect(hwnd, &mut rect) };
        let cx = (rect.left + rect.right) / 2;
        let cy = (rect.top + rect.bottom) / 2;
        println!("在窗口中心 ({cx},{cy}) 点击");
        click_at(cx, cy);
        std::thread::sleep(std::time::Duration::from_millis(600));
        let _ = shoot(hwnd, "click_after.png");
        return;
    }

    println!("窗口已在前台，准备输入 {first:?}");
    let sent = type_text(&first);
    println!("已发送 {sent} 个字符");
    std::thread::sleep(std::time::Duration::from_millis(600));

    if shoot(hwnd, "type_test.png") {
        println!("已抓图 type_test.png");
    } else {
        eprintln!("抓图失败");
    }
}
