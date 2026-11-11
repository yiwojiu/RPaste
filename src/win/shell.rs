//! 用系统默认程序打开一个本地路径或网址。
//!
//! 关于面板里「数据目录」「项目主页」两行点一下就走这里。不自己起
//! `explorer.exe`：`ShellExecuteW` 会把默认关联、开新窗口、已有窗口复用
//! 这些事全交给系统，行为跟用户在资源管理器里双击一致。

use std::ptr;

use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub fn open(target: &str) -> bool {
    let op: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();

    // 返回值是 HINSTANCE，但这里是拿它当状态码用：> 32 才算成功，
    // 小于等于 32 的值分别是各种错误（找不到文件、没有关联程序……）。
    let ret = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;

    if ret > 32 {
        true
    } else {
        crate::log_warn!("打开 {target} 失败（ShellExecuteW 返回 {ret}）");
        false
    }
}
