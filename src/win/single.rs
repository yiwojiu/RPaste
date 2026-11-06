//! 单实例保护。
//!
//! 剪贴板工具必须只有一个实例：两个进程各自监听 `WM_CLIPBOARDUPDATE`，
//! 会把同一次复制记两遍，还会互相抢 `OpenClipboard`。
//!
//! 用命名互斥量而不是「找窗口」之类的土办法：互斥量由内核持有，
//! 进程崩溃也会自动释放，不存在残留判断错误的问题。

use std::sync::atomic::{AtomicPtr, Ordering};

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Threading::CreateMutexW;

/// 互斥量名字。`Local\` 前缀限定在当前登录会话内——
/// 用 `Global\` 的话多用户环境下会互相挡住。
const MUTEX_NAME: &str = "Local\\RPaste_SingleInstance_v1";

/// 持有互斥量句柄，直到进程结束。
static MUTEX_HANDLE: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// 返回 true 表示可以继续启动；false 表示已经有实例在跑。
///
/// 创建失败时**放行**：宁可多开一个，也别因为系统异常把用户挡在门外。
pub fn acquire() -> bool {
    let name: Vec<u16> = MUTEX_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if handle.is_null() {
            return true;
        }

        if GetLastError() == ERROR_ALREADY_EXISTS {
            return false;
        }

        // 故意不关闭：句柄活到进程结束，系统自动回收。
        // 提前 CloseHandle 会让互斥量失去持有者，单实例就失效了。
        MUTEX_HANDLE.store(handle, Ordering::Relaxed);
        true
    }
}
