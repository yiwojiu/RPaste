//! 系统托盘图标。
//!
//! 托盘图标挂在 listener 线程的隐藏窗口上（那个窗口本来就有消息循环），
//! 右键菜单用 `TrackPopupMenu` 弹出。菜单是模态的，会短暂占用 listener 线程——
//! 用户操作托盘时不会同时复制，这个阻塞可以接受。
//!
//! 图标暂用系统默认的 `IDI_APPLICATION`，M4 再换成自己的资源图标。

use std::ptr;

use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, DestroyMenu, GetCursorPos,
    GetSystemMetrics, IDI_APPLICATION, LR_DEFAULTCOLOR, LoadIconW, MF_CHECKED, MF_SEPARATOR,
    MF_STRING, SM_CXSMICON, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
    WM_LBUTTONUP, WM_RBUTTONUP,
};

/// 编译期嵌入的多尺寸图标，见 `examples/gen_icon.rs`。
const ICON_ICO: &[u8] = include_bytes!("../../assets/icon.ico");

/// 托盘回调消息。`WM_APP + 1`，避开系统消息区间。
pub const TRAY_MSG: u32 = 0x8000 + 1;

const TRAY_UID: u32 = 1;
const MENU_TOGGLE: usize = 1;
const MENU_SETTINGS: usize = 2;
const MENU_AUTOSTART: usize = 3;
const MENU_ABOUT: usize = 4;
const MENU_QUIT: usize = 5;

/// 当前版本，取自 `Cargo.toml`。
///
/// 发版只需要改 `Cargo.toml` 一处，托盘菜单、悬停提示、关于面板会一起跟上——
/// 手工维护三份字符串迟早会漏一个。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 用户在托盘菜单里选了什么，或者点了图标本身。
pub enum TrayAction {
    Toggle,
    /// 唤出窗口并打开设置面板。
    Settings,
    /// 唤出窗口并打开关于面板。
    About,
    /// 切换开机自启。
    ToggleAutostart,
    Quit,
    None,
}

/// 从 ICO 里挑一个最接近系统所需尺寸的条目。
///
/// `CreateIconFromResourceEx` 要的是**单个图标位图**，不是整个 ICO 文件，
/// 所以得自己拆 ICONDIR 表。条目内是 PNG（Vista 起支持）。
fn pick_ico_entry(ico: &[u8], want: u32) -> Option<(&[u8], u32)> {
    if ico.len() < 6 || ico[0] != 0 || ico[1] != 0 || ico[2] != 1 || ico[3] != 0 {
        return None;
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    if ico.len() < 6 + count * 16 {
        return None;
    }

    let mut best: Option<(u32, usize, usize)> = None;
    for i in 0..count {
        let e = 6 + i * 16;
        // 0 表示 256
        let dim = if ico[e] == 0 { 256 } else { ico[e] as u32 };
        let len = u32::from_le_bytes(ico[e + 8..e + 12].try_into().ok()?) as usize;
        let off = u32::from_le_bytes(ico[e + 12..e + 16].try_into().ok()?) as usize;
        if off + len > ico.len() {
            continue;
        }
        let better = match best {
            None => true,
            Some((bs, _, _)) => dim.abs_diff(want) < bs.abs_diff(want),
        };
        if better {
            best = Some((dim, off, len));
        }
    }

    best.map(|(dim, off, len)| (&ico[off..off + len], dim))
}

/// 加载托盘图标，尺寸跟随系统 DPI（高 DPI 下要 24/32px，硬塞 16px 会糊）。
fn load_icon() -> isize {
    let want = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;

    if let Some((data, dim)) = pick_ico_entry(ICON_ICO, want) {
        let icon = unsafe {
            CreateIconFromResourceEx(
                data.as_ptr(),
                data.len() as u32,
                1,           // fIcon = TRUE
                0x0003_0000, // 版本 3.0
                dim as i32,
                dim as i32,
                LR_DEFAULTCOLOR,
            )
        };
        if !icon.is_null() {
            return icon as isize;
        }
        crate::log_warn!(
            "从 ICO 创建图标失败（{dim}px，{} 字节），回退到系统默认图标",
            data.len()
        );
    } else {
        crate::log_warn!("ICO 解析失败，回退到系统默认图标");
    }

    // 兜底：图标解析失败也别让托盘空着
    unsafe { LoadIconW(ptr::null_mut(), IDI_APPLICATION) as isize }
}

pub fn add(hwnd: HWND) -> bool {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = TRAY_MSG;
    nid.hIcon = load_icon() as *mut _;

    // 版本号放进悬停提示：想看版本时不必先点开菜单。
    let tip: Vec<u16> = format!("R-Paste 剪贴板历史 v{VERSION}\0")
        .encode_utf16()
        .collect();
    let n = tip.len().min(nid.szTip.len());
    nid.szTip[..n].copy_from_slice(&tip[..n]);

    unsafe { Shell_NotifyIconW(NIM_ADD, &nid) != 0 }
}

pub fn remove(hwnd: HWND) {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// 处理托盘窗口收到的鼠标消息。左键单击切窗口，右键弹菜单。
pub fn on_mouse(hwnd: HWND, lparam: isize) -> TrayAction {
    match lparam as u32 {
        WM_LBUTTONUP => TrayAction::Toggle,
        WM_RBUTTONUP => show_menu(hwnd),
        _ => TrayAction::None,
    }
}

fn show_menu(hwnd: HWND) -> TrayAction {
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return TrayAction::None;
        }

        let toggle: Vec<u16> = "显示 / 隐藏\0".encode_utf16().collect();
        let settings: Vec<u16> = "设置…\0".encode_utf16().collect();
        // 勾选状态直接读注册表，不维护自己的副本——用户可能在别处改过
        let autostart: Vec<u16> = "开机自启\0".encode_utf16().collect();
        // 版本直接写进菜单项：用户第一眼想看的就是「我装的是哪个版本」，
        // 不该还得再点一层。
        let about: Vec<u16> = format!("关于 R-Paste v{VERSION}…\0").encode_utf16().collect();
        let quit: Vec<u16> = "退出\0".encode_utf16().collect();

        AppendMenuW(menu, MF_STRING, MENU_TOGGLE, toggle.as_ptr());
        AppendMenuW(menu, MF_STRING, MENU_SETTINGS, settings.as_ptr());

        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());

        let auto_flags = if crate::win::autostart::is_enabled() {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        AppendMenuW(menu, auto_flags, MENU_AUTOSTART, autostart.as_ptr());

        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_ABOUT, about.as_ptr());
        AppendMenuW(menu, MF_STRING, MENU_QUIT, quit.as_ptr());

        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);

        // 没有 SetForegroundWindow 的话，菜单点外面不会消失，会一直挂着。
        SetForegroundWindow(hwnd);

        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            hwnd,
            ptr::null(),
        );
        DestroyMenu(menu);

        match cmd as usize {
            MENU_TOGGLE => TrayAction::Toggle,
            MENU_SETTINGS => TrayAction::Settings,
            MENU_AUTOSTART => TrayAction::ToggleAutostart,
            MENU_ABOUT => TrayAction::About,
            MENU_QUIT => TrayAction::Quit,
            _ => TrayAction::None,
        }
    }
}
