//! 开机自启：写 `HKCU\...\CurrentVersion\Run`。
//!
//! 用注册表而不是「启动」文件夹里的快捷方式：
//! 注册表一条 `REG_SZ` 就够，不需要创建 `.lnk`（那要走 COM，麻烦得多）。
//!
//! 只写当前用户（HKCU），不需要管理员权限。

use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "RPaste";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_key(access: u32) -> Option<HKEY> {
    let sub = wide(RUN_KEY);
    let mut key: HKEY = ptr::null_mut();
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, access, &mut key) };
    if rc == ERROR_SUCCESS { Some(key) } else { None }
}

/// 读注册表里已登记的命令行。
fn registered_command() -> Option<String> {
    let key = open_key(KEY_QUERY_VALUE)?;
    let name = wide(VALUE_NAME);

    // 先问长度，再按长度分配
    let mut len: u32 = 0;
    let rc = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut len,
        )
    };
    if rc != ERROR_SUCCESS || len == 0 {
        unsafe { windows_sys::Win32::System::Registry::RegCloseKey(key) };
        return None;
    }

    let mut buf = vec![0u8; len as usize];
    let rc = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            buf.as_mut_ptr(),
            &mut len,
        )
    };
    unsafe { windows_sys::Win32::System::Registry::RegCloseKey(key) };

    if rc != ERROR_SUCCESS {
        return None;
    }

    let units: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// 当前是否已开启自启。
///
/// 判断标准是「值存在**且**指向当前这个 exe」，不是单看值存在。
/// 只查存在的话，从别的目录运行时会显示已勾选，但实际指向之前那份 exe：
/// 用户以为自启有效，关掉时又会把别处的登记删掉。
pub fn is_enabled() -> bool {
    let Some(registered) = registered_command() else {
        return false;
    };
    let Some(current) = exe_command() else {
        return false;
    };
    // Windows 路径大小写不敏感
    registered.trim().eq_ignore_ascii_case(current.trim())
}

/// 注册表里的值：可执行文件完整路径。路径含空格必须加引号，
/// 否则 Windows 会把空格前那截当成程序名去启动。
fn exe_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\"", exe.display()))
}

pub fn enable() -> Result<(), String> {
    let cmd = exe_command().ok_or("拿不到自己的路径")?;
    let units: Vec<u16> = cmd.encode_utf16().collect();
    let bytes = units.len() * 2;

    let key = open_key(KEY_SET_VALUE).ok_or("打不开 Run 注册表项")?;
    let name = wide(VALUE_NAME);

    let rc = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_SZ,
            units.as_ptr() as *const u8,
            bytes as u32,
        )
    };
    unsafe { windows_sys::Win32::System::Registry::RegCloseKey(key) };

    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("写入注册表失败，错误码 {rc}"))
    }
}

pub fn disable() -> Result<(), String> {
    let key = open_key(KEY_SET_VALUE).ok_or("打不开 Run 注册表项")?;
    let name = wide(VALUE_NAME);

    let rc = unsafe { RegDeleteValueW(key, name.as_ptr()) };
    unsafe { windows_sys::Win32::System::Registry::RegCloseKey(key) };

    // 本来就不存在也算成功
    if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("删除注册表项失败，错误码 {rc}"))
    }
}

/// 切换开关，返回切换后的状态。
pub fn toggle() -> Result<bool, String> {
    if is_enabled() {
        disable()?;
        Ok(false)
    } else {
        enable()?;
        Ok(true)
    }
}
