//! 诊断：打印当前前台进程的识别结果。
//!
//! 用来确认 `win::appinfo::foreground_app` 在真实环境里能不能拿到名字——
//! 这个函数内部有好几层 Win32 调用，任何一层失败都只会静默返回 `None`，
//! 不单独验证的话在日志里只表现为「来源列为空」，看不出是哪一步没成。

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

fn main() {
    unsafe {
        let hwnd = GetForegroundWindow();
        println!("GetForegroundWindow = {hwnd:?}");
        if hwnd.is_null() {
            return;
        }

        let mut pid = 0u32;
        let tid = GetWindowThreadProcessId(hwnd, &mut pid);
        println!("tid = {tid}, pid = {pid}");

        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        println!("OpenProcess = {handle:?}");
        if handle.is_null() {
            println!("last_error = {}", std::io::Error::last_os_error());
            return;
        }

        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
        println!("QueryFullProcessImageNameW ok={ok} len={len}");
        CloseHandle(handle);

        if ok != 0 && len > 0 {
            let path = OsString::from_wide(&buf[..len as usize]);
            println!("path = {}", path.to_string_lossy());
        }

        println!(
            "appinfo 结果 = {:?}",
            rpaste::win::appinfo::foreground_app()
        );
    }
}
