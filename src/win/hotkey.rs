//! 全局热键：默认 `Alt+V` 呼出 / 隐藏窗口，可在设置面板里改。
//!
//! 用 `RegisterHotKey`（系统级热键）而不是低级键盘钩子：
//! 低级钩子会拖慢全系统的键盘输入，还会被安全软件盯上。
//! `MOD_NOREPEAT` 保证按住不放只触发一次，不会连发。
//!
//! 热键注册在 listener 线程的 message-only 窗口上——那个窗口本来就有
//! 独立消息循环，收到 `WM_HOTKEY` 后通过 channel 通知主线程，
//! 完全绕开 eframe/winit 的窗口句柄，不依赖 GUI 框架的底层实现细节。
//!
//! **改键必须回到 listener 线程**：`RegisterHotKey` / `UnregisterHotKey`
//! 的登记是挂在「调用它的线程」上的，跨线程撤销会失败。所以 UI 改完注册表
//! 之后只是往 listener 窗口 `PostMessageW`，真正的一撤一注册在那里做。

use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use eframe::egui;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey, UnregisterHotKey,
};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use super::settings;

/// 正式注册用的热键 id。系统按 (窗口, id) 区分，与具体组合键无关。
/// `pub` 是因为 `WM_HOTKEY` 的 `wParam` 就是它，listener 要拿它比对；
/// 两处各写一个常量迟早会漂。
pub const ID: i32 = 0x52; // 'R' = R-Paste
/// 探测时用的另一个 id：同一个 id 重复注册会被系统当成「自己的同一个热键」，
/// 探测就没法测出「是否被别人占用」。
const PROBE_ID: i32 = 0x53;

const VK_V: u32 = 0x56;

/// 让监听线程按注册表重新注册热键的窗口消息（`WM_APP + 2`）。
///
/// **必须避开 `tray::TRAY_MSG`（`WM_APP + 1`）**：同一个窗口的 wndproc 里
/// 两个分支取同一个值，后写的那个永远收不到消息。
pub const WM_RELOAD_HOTKEY: u32 = 0x8000 + 2;

/// 当前生效的热键。只有 listener 线程会写，所以加锁只是为了跨线程读。
static ACTIVE: Mutex<Option<Hotkey>> = Mutex::new(None);
/// listener 的隐藏窗口句柄，用来把「改键」这件事投递过去。
static LISTENER: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());
/// 设置面板是否正在录制新组合。
static CAPTURING: AtomicBool = AtomicBool::new(false);

/// 录制状态由 UI 每帧同步过来。
///
/// 用途很具体：录制时用户可能正好按下**当前生效**的那个组合，系统会先把它
/// 从键盘上吞掉、转成 `WM_HOTKEY` 发给 listener。若不拦住，窗口会在录制
/// 中途被收起来，用户完全不知道发生了什么。
pub fn set_capturing(on: bool) {
    CAPTURING.store(on, Ordering::Relaxed);
}

pub fn is_capturing() -> bool {
    CAPTURING.load(Ordering::Relaxed)
}

/// 一个「修饰键 + 主键」组合。
///
/// 存的是 Win32 的原始值（`MOD_*` 位与虚拟键码），不是字符串：注册和探测都
/// 要这俩数字，中间转成文字再解析回来只会多一条出错路径。显示时才 `label()`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hotkey {
    mods: u32,
    vk: u32,
}

impl Hotkey {
    /// 默认 `Alt+V`。
    pub const DEFAULT: Self = Self {
        mods: MOD_ALT,
        vk: VK_V,
    };

    /// 从注册表读。读不到或内容不合法（例如注册表被手改坏了）就回默认值——
    /// 宁可悄悄回到默认，也不要留一个永远注册不上的组合。
    pub fn load() -> Self {
        let (mods, vk) = settings::hotkey();
        let hk = Self { mods, vk };
        if hk.valid() { hk } else { Self::DEFAULT }
    }

    /// 从一次按键事件里取组合键。
    ///
    /// 没有修饰键或键位不支持返回 `None`——调用方据此继续等下一次按键，
    /// 而不是把一个「单按 F5」当成合法热键（那样会直接把 F5 从所有程序手里抢走）。
    pub fn from_key(key: egui::Key, mods: egui::Modifiers) -> Option<Self> {
        let mut flags = 0;
        if mods.ctrl {
            flags |= MOD_CONTROL;
        }
        if mods.alt {
            flags |= MOD_ALT;
        }
        if mods.shift {
            flags |= MOD_SHIFT;
        }
        if flags == 0 {
            return None;
        }
        let hk = Self {
            mods: flags,
            vk: vk_of(key.name())?,
        };
        hk.valid().then_some(hk)
    }

    /// 至少要有一个修饰键：`RegisterHotKey` 允许注册裸按键，但那会把
    /// 这个键从所有程序手里抢走（连打字都不行），不提供这个口子。
    ///
    /// Win 键不在可选范围内：egui 拿不到 Win 的按下状态，而且 Win 组合键
    /// 大量被 Shell 占用，注册成功率很低。
    fn valid(&self) -> bool {
        self.mods & (MOD_CONTROL | MOD_ALT | MOD_SHIFT) != 0 && vk_name(self.vk).is_some()
    }

    /// 显示用文字，例如 `Ctrl+Alt+V`。
    pub fn label(&self) -> String {
        let mut s = String::with_capacity(16);
        for (flag, name) in [
            (MOD_CONTROL, "Ctrl"),
            (MOD_ALT, "Alt"),
            (MOD_SHIFT, "Shift"),
        ] {
            if self.mods & flag != 0 {
                s.push_str(name);
                s.push('+');
            }
        }
        s.push_str(vk_name(self.vk).unwrap_or("?"));
        s
    }
}

// ------------------------------------------------------------ 键名与虚拟键码

/// egui 的键名 → Win32 虚拟键码。
///
/// 表只覆盖「适合做全局热键」的键：字母、数字、F1–F24、方向、编辑键与标点。
/// `Escape` 故意不在表里——它是设置面板的取消键，注册成热键就永远关不掉面板了。
fn vk_of(name: &str) -> Option<u32> {
    // 字母和数字：键名就是单个 ASCII 字符，虚拟键码等于其大写形式。
    let bytes = name.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_alphanumeric() {
        return Some(bytes[0].to_ascii_uppercase() as u32);
    }

    let vk = match name {
        "Space" => 0x20,
        "Enter" => 0x0D,
        "Tab" => 0x09,
        "Backspace" => 0x08,
        "Insert" => 0x2D,
        "Delete" => 0x2E,
        "Home" => 0x24,
        "End" => 0x23,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "Left" => 0x25,
        "Up" => 0x26,
        "Right" => 0x27,
        "Down" => 0x28,
        "Backtick" => 0xC0,
        "Minus" => 0xBD,
        "Equals" => 0xBB,
        "OpenBracket" => 0xDB,
        "CloseBracket" => 0xDD,
        "Semicolon" => 0xBA,
        "Quote" => 0xDE,
        "Comma" => 0xBC,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "Backslash" => 0xDC,
        "Plus" => 0xBB,
        _ => {
            // F1–F24：0x70 起连续排列。
            let n: u32 = name.strip_prefix('F')?.parse().ok()?;
            return (1..=24).contains(&n).then_some(0x6F + n);
        }
    };
    Some(vk)
}

/// 虚拟键码 → 显示名。反向的 `vk_of`，只覆盖 `vk_of` 能产出的那些码。
fn vk_name(vk: u32) -> Option<&'static str> {
    const NAMED: &[(u32, &str)] = &[
        (0x20, "Space"),
        (0x0D, "Enter"),
        (0x09, "Tab"),
        (0x08, "Backspace"),
        (0x2D, "Insert"),
        (0x2E, "Delete"),
        (0x24, "Home"),
        (0x23, "End"),
        (0x21, "PgUp"),
        (0x22, "PgDn"),
        (0x25, "\u{2190}"),
        (0x26, "\u{2191}"),
        (0x27, "\u{2192}"),
        (0x28, "\u{2193}"),
        (0xC0, "`"),
        (0xBD, "-"),
        (0xBB, "="),
        (0xDB, "["),
        (0xDD, "]"),
        (0xBA, ";"),
        (0xDE, "'"),
        (0xBC, ","),
        (0xBE, "."),
        (0xBF, "/"),
        (0xDC, "\\"),
    ];

    // 字母和数字的显示名是单个字符，静态表写不下（要 36 项），单独判断。
    if (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk) {
        // 这里返回的是 `&'static str`，没法从数字现场造字符串，
        // 所以用一张穷举表——36 个字符，编译器会合并成常量。
        const LETTERS: [&str; 26] = [
            "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q",
            "R", "S", "T", "U", "V", "W", "X", "Y", "Z",
        ];
        const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
        return if vk <= 0x39 {
            Some(DIGITS[(vk - 0x30) as usize])
        } else {
            Some(LETTERS[(vk - 0x41) as usize])
        };
    }
    if (0x70..=0x87).contains(&vk) {
        const FKEYS: [&str; 24] = [
            "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13",
            "F14", "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
        ];
        return Some(FKEYS[(vk - 0x70) as usize]);
    }
    NAMED
        .iter()
        .find(|(code, _)| *code == vk)
        .map(|(_, name)| *name)
}

// ------------------------------------------------------------ 注册与改键

/// listener 建好隐藏窗口后调一次，之后 `request_reload` 才知道往哪投递。
pub fn set_listener(hwnd: HWND) {
    LISTENER.store(hwnd, Ordering::Relaxed);
}

/// 请 listener 线程按注册表重新注册热键。
///
/// 只是投递，不等结果：注册失败的兜底逻辑在 `apply` 里（回滚到旧组合），
/// UI 侧已经用 `available()` 提前拦过一次，剩下的是极窄的竞态窗口。
pub fn request_reload() {
    let hwnd = LISTENER.load(Ordering::Relaxed);
    if hwnd.is_null() {
        return;
    }
    unsafe {
        let _ = PostMessageW(hwnd, WM_RELOAD_HOTKEY, 0, 0);
    }
}

/// 按注册表注册热键。启动时由 listener 调用。
///
/// # Safety
/// `hwnd` 必须是有效的窗口句柄。这里只把它透传给系统 API，不解引用。
pub unsafe fn register(hwnd: HWND) -> Result<(), String> {
    unsafe { apply(hwnd, Hotkey::load()) }
}

/// 重新读注册表并切换热键。当前组合已失效时回滚到旧的。
///
/// # Safety
/// 同 `register`。
pub unsafe fn reload(hwnd: HWND) -> Result<(), String> {
    unsafe { apply(hwnd, Hotkey::load()) }
}

/// 一撤一注册。**先注册新的不行**——两次注册用的是同一个 id，而新组合
/// 此刻可能正被别人占着，失败后旧组合已经被撤掉，用户按哪儿都没反应。
/// 所以顺序是「撤旧 → 注册新 → 失败则把旧的注册回去」。
unsafe fn apply(hwnd: HWND, hk: Hotkey) -> Result<(), String> {
    let old = *ACTIVE.lock().unwrap();
    if old == Some(hk) {
        return Ok(());
    }

    unsafe { UnregisterHotKey(hwnd, ID) };
    let ok = unsafe { RegisterHotKey(hwnd, ID, hk.mods | MOD_NOREPEAT, hk.vk) };
    if ok == 0 {
        // 回滚。旧组合通常仍可用（它刚被自己撤掉），失败只可能是被别的程序
        // 抢在那几毫秒里注册走了，那就真的没辙了，只能记日志。
        if let Some(old) = old {
            let back = unsafe { RegisterHotKey(hwnd, ID, old.mods | MOD_NOREPEAT, old.vk) };
            if back == 0 {
                crate::log_error!("热键回滚失败：{} 无法恢复", old.label());
            }
        }
        *ACTIVE.lock().unwrap() = old;
        return Err(format!("{} 注册失败（可能被其他程序占用）", hk.label()));
    }

    *ACTIVE.lock().unwrap() = Some(hk);
    Ok(())
}

/// 这个组合现在能不能注册上。
///
/// 从 UI 线程做一次「注册后立刻撤销」的探测，用的 id 和正式注册不同，
/// 所以正在生效的那个组合不会互相干扰。能提前把「被占用」告诉用户，
/// 而不是让他改完发现没反应。
pub fn available(hk: Hotkey) -> bool {
    let Some(hwnd) = super::window::main_window() else {
        return false;
    };
    unsafe {
        let ok = RegisterHotKey(hwnd, PROBE_ID, hk.mods | MOD_NOREPEAT, hk.vk);
        if ok == 0 {
            return false;
        }
        UnregisterHotKey(hwnd, PROBE_ID);
        true
    }
}

/// 改键的完整流程：校验 → 探测 → 落注册表 → 请 listener 重新注册。
///
/// 调用方在 UI 线程。`Ok` 只代表「注册表已写、重注册请求已投递」，
/// 真正的注册发生在 listener 线程上，失败会回滚并写进日志。
pub fn try_apply(hk: Hotkey) -> Result<(), String> {
    if !hk.valid() {
        return Err("需要 Ctrl / Alt / Shift 加一个普通键".to_owned());
    }
    if !available(hk) {
        return Err(format!("{} 已被其他程序占用", hk.label()));
    }
    settings::set_hotkey(hk.mods, hk.vk)?;
    request_reload();
    Ok(())
}

/// # Safety
/// `hwnd` 必须是之前成功注册过热键的窗口句柄。
pub unsafe fn unregister(hwnd: HWND) {
    unsafe {
        UnregisterHotKey(hwnd, ID);
    }
    *ACTIVE.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_alt_v() {
        assert_eq!(Hotkey::DEFAULT.label(), "Alt+V");
        assert!(Hotkey::DEFAULT.valid());
    }

    #[test]
    fn label_orders_modifiers_as_ctrl_alt_shift() {
        let hk = Hotkey {
            mods: MOD_SHIFT | MOD_ALT | MOD_CONTROL,
            vk: VK_V,
        };
        assert_eq!(hk.label(), "Ctrl+Alt+Shift+V");
    }

    #[test]
    fn named_keys_round_trip() {
        // vk_of 与 vk_name 必须互为逆运算，否则「设置里显示 A，按下却是别的键」。
        for name in [
            "Space",
            "Enter",
            "Tab",
            "Insert",
            "Delete",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "Backtick",
            "Minus",
            "Equals",
            "Semicolon",
            "Comma",
            "Period",
            "Slash",
            "Backslash",
            "OpenBracket",
            "CloseBracket",
            "Quote",
        ] {
            let vk = vk_of(name).unwrap_or_else(|| panic!("{name} 没有映射"));
            assert!(vk_name(vk).is_some(), "{name} 反向查不到");
        }
    }

    #[test]
    fn letters_digits_and_function_keys_round_trip() {
        for name in ["A", "Z", "0", "9", "F1", "F12", "F24"] {
            let vk = vk_of(name).unwrap_or_else(|| panic!("{name} 没有映射"));
            assert_eq!(vk_name(vk).unwrap(), name);
        }
        // 大写小写都是同一个物理键。
        assert_eq!(vk_of("v"), vk_of("V"));
        // 超出 F24 的不认。
        assert_eq!(vk_of("F25"), None);
    }

    #[test]
    fn escape_is_not_a_usable_hotkey() {
        // Esc 是设置面板的取消键，注册成热键就再也关不掉了。
        assert_eq!(vk_of("Escape"), None);
    }

    #[test]
    fn no_modifier_means_none() {
        let none = egui::Modifiers::NONE;
        assert!(Hotkey::from_key(egui::Key::V, none).is_none());
        let hk = Hotkey::from_key(egui::Key::V, egui::Modifiers::ALT).unwrap();
        assert_eq!(hk.label(), "Alt+V");
    }
}
