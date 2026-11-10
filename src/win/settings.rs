//! 少量持久化开关，存 `HKCU\Software\RPaste`（`REG_DWORD`）。
//!
//! 用注册表而不是配置文件：要存的只是个位数的布尔量，为它引入一套
//! 「读文件-解析-容错-原子写」的机制不划算。写 HKCU 不需要管理员权限。

use std::ptr;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE,
    RegCloseKey, RegCreateKeyExW, RegQueryValueExW, RegSetValueExW,
};

const KEY: &str = r"Software\RPaste";
/// `1` = 接管 Win+V（默认），`0` = 交还系统剪贴板历史。
const VALUE_WIN_V: &str = "TakeOverWinV";
/// 呼出热键的修饰键位（`MOD_ALT` = 1、`MOD_CONTROL` = 2、`MOD_SHIFT` = 4）。
const VALUE_HOTKEY_MODS: &str = "HotkeyMods";
/// 呼出热键的虚拟键码（默认 0x56 = `V`）。
const VALUE_HOTKEY_VK: &str = "HotkeyVk";
/// 上次关闭时的窗口逻辑尺寸（像素）。两个值分开存，缺一个就当没存过。
const VALUE_WINDOW_W: &str = "WindowWidth";
const VALUE_WINDOW_H: &str = "WindowHeight";
/// 历史条目上限。0 或缺省时用 `DEFAULT_MAX_ENTRIES`。
const VALUE_MAX_ENTRIES: &str = "MaxEntries";

/// 默认热键 `Alt+V`，与 `hotkey::Hotkey::DEFAULT` 保持一致。
const DEFAULT_HOTKEY: (u32, u32) = (1, 0x56);

/// 默认条目上限，与 `store::DEFAULT_MAX_ENTRIES` 保持一致。
const DEFAULT_MAX_ENTRIES: i64 = crate::store::DEFAULT_MAX_ENTRIES;

/// 允许设置的条目上限区间。
///
/// 下限 100：再小会变成「刚复制几条就被删」，用户只会觉得程序坏了。
/// 上限 20 万：纯文本场景约几百 MB，还能接受；图片场景早就爆了，那是
/// 用户自己的选择，但得先做出「占用可见」才谈得上更放开（目前没有）。
pub const MAX_ENTRIES_RANGE: (i64, i64) = (100, 200_000);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_key(access: u32, create: bool) -> Option<HKEY> {
    let sub = wide(KEY);
    let mut key: HKEY = ptr::null_mut();
    let rc = unsafe {
        if create {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                sub.as_ptr(),
                0,
                ptr::null(),
                REG_OPTION_NON_VOLATILE,
                access,
                ptr::null_mut(),
                &mut key,
                ptr::null_mut(),
            )
        } else {
            windows_sys::Win32::System::Registry::RegOpenKeyExW(
                HKEY_CURRENT_USER,
                sub.as_ptr(),
                0,
                access,
                &mut key,
            )
        }
    };
    if rc == ERROR_SUCCESS { Some(key) } else { None }
}

fn read_dword(name: &str) -> Option<u32> {
    let key = open_key(KEY_QUERY_VALUE, false)?;
    let n = wide(name);
    let mut value: u32 = 0;
    let mut len = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegQueryValueExW(
            key,
            n.as_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut value as *mut u32 as *mut u8,
            &mut len,
        )
    };
    unsafe { RegCloseKey(key) };
    if rc == ERROR_SUCCESS && len == 4 {
        Some(value)
    } else {
        None
    }
}

fn write_dword(name: &str, value: u32) -> Result<(), String> {
    let key =
        open_key(KEY_QUERY_VALUE | KEY_SET_VALUE, true).ok_or("无法创建 HKCU\\Software\\RPaste")?;
    let n = wide(name);
    let rc = unsafe {
        RegSetValueExW(
            key,
            n.as_ptr(),
            0,
            REG_DWORD,
            &value as *const u32 as *const u8,
            std::mem::size_of::<u32>() as u32,
        )
    };
    unsafe { RegCloseKey(key) };
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("写注册表失败，错误码 {rc}"))
    }
}

/// 是否接管 `Win+V`。默认开启——用户想用这个工具，多半就是想摆脱系统面板。
pub fn win_v_takeover() -> bool {
    read_dword(VALUE_WIN_V).unwrap_or(1) != 0
}

pub fn set_win_v_takeover(on: bool) -> Result<(), String> {
    write_dword(VALUE_WIN_V, on as u32)
}

/// 呼出热键：`(修饰键位, 虚拟键码)`。
///
/// 读不到就给默认值。两个值必须成对出现，所以这里没有「半个热键」的中间态。
pub fn hotkey() -> (u32, u32) {
    let mods = read_dword(VALUE_HOTKEY_MODS).unwrap_or(DEFAULT_HOTKEY.0);
    let vk = read_dword(VALUE_HOTKEY_VK).unwrap_or(DEFAULT_HOTKEY.1);
    (mods, vk)
}

pub fn set_hotkey(mods: u32, vk: u32) -> Result<(), String> {
    write_dword(VALUE_HOTKEY_MODS, mods)?;
    write_dword(VALUE_HOTKEY_VK, vk)
}

/// 上次的窗口逻辑尺寸，没存过或数值离谱就返回 `None`（调用方用默认值）。
pub fn window_size() -> Option<(u32, u32)> {
    let w = read_dword(VALUE_WINDOW_W)?;
    let h = read_dword(VALUE_WINDOW_H)?;
    // 320 是 `min_inner_size`；比它还小的值只可能来自手动改注册表，
    // 采信它会让窗口开在一个拖不动也看不全的尺寸上。
    (w >= 320 && h >= 320).then_some((w, h))
}

pub fn set_window_size(w: u32, h: u32) -> Result<(), String> {
    write_dword(VALUE_WINDOW_W, w)?;
    write_dword(VALUE_WINDOW_H, h)
}

/// 历史条目上限。
///
/// 读到的值超出 `MAX_ENTRIES_RANGE` 时夹到边界，而不是拒绝——用户手改注册表
/// 填了 999999 说明他想要「尽量多」，给 20 万比给默认 1 万更贴近意图；
/// 填 0 则是笔误或者想「不限」，夹到 100 条至少不会一条不剩。
pub fn max_entries() -> i64 {
    let value = read_dword(VALUE_MAX_ENTRIES)
        .map(|v| v as i64)
        .unwrap_or(DEFAULT_MAX_ENTRIES);
    value.clamp(MAX_ENTRIES_RANGE.0, MAX_ENTRIES_RANGE.1)
}

pub fn set_max_entries(n: i64) -> Result<(), String> {
    let n = n.clamp(MAX_ENTRIES_RANGE.0, MAX_ENTRIES_RANGE.1);
    write_dword(VALUE_MAX_ENTRIES, n as u32)
}
