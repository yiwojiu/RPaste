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
