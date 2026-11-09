//! 抓「这次复制是从哪个程序来的」。
//!
//! 剪贴板格式里没有任何字段记录来源，只能靠时间相关性推断：`WM_CLIPBOARDUPDATE`
//! 到达时，前台窗口还停在用户按下 Ctrl+C 的那个程序上。所以这个函数必须在
//! **监听线程的回调里**调用，不能等事件传到 worker 再问——那时前台可能已经
//! 换了（比如用户按完就切了窗口，或者自动粘贴把焦点还回去了）。
//!
//! 只在报告里放一个短名字，不存完整路径：路径里有用户名，属于隐私，而且界面上
//! 也放不下。查进程名需要 `PROCESS_QUERY_LIMITED_INFORMATION`，这是权限最低的
//! 查询权限，不触发 UAC，也能读到大部分进程（少数保护进程读不到，返回 `None`
//! 即可，不值得为此降级）。

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// 前台进程的可读名字，例如 `Chrome` / `VS Code` / `终端`。
///
/// 取不到就返回 `None`——来源只是辅助信息，任何一步失败都不该影响捕获本身。
pub fn foreground_app() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }

        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return None;
        }
        // 自己复制自己（在搜索框里 Ctrl+C、或第二个实例唤出主窗口）不该记来源。
        // 用 PID 比用进程名可靠：改名、带连字符的写法、大小写都不会漏。
        if pid == GetCurrentProcessId() {
            return None;
        }

        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }

        // 512 个 UTF-16 足够放下任何真实路径；真超了会被系统拒绝，返回 0。
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(handle);

        if ok == 0 || len == 0 {
            return None;
        }

        let path = OsString::from_wide(&buf[..len as usize]);
        let exe = std::path::Path::new(&path).file_name()?.to_string_lossy();
        Some(friendly(&exe))
    }
}

/// exe 文件名 → 界面上显示的名字。
///
/// 硬编码表只覆盖「用户一眼能认出来、但 exe 名认不出来」的那些。不在这里的
/// 一律用去掉扩展名的原名——那通常已经是 `Everything`、`Obsidian` 这种可读名，
/// 硬猜反而会猜错。
fn friendly(exe: &str) -> String {
    let stem = exe
        .strip_suffix(".exe")
        .or_else(|| exe.strip_suffix(".EXE"))
        .unwrap_or(exe);

    match stem.to_ascii_lowercase().as_str() {
        "chrome" => "Chrome".into(),
        "msedge" => "Edge".into(),
        "firefox" => "Firefox".into(),
        "code" => "VS Code".into(),
        "code - insiders" => "VS Code".into(),
        "devenv" => "Visual Studio".into(),
        "idea64" | "idea" => "IDEA".into(),
        "clion64" | "clion" => "CLion".into(),
        "pycharm64" | "pycharm" => "PyCharm".into(),
        "goland64" | "goland" => "GoLand".into(),
        "webstorm64" | "webstorm" => "WebStorm".into(),
        "windowsterminal" => "终端".into(),
        "conhost" => "终端".into(),
        "openconsole" => "终端".into(),
        "powershell" => "PowerShell".into(),
        "pwsh" => "PowerShell".into(),
        "cmd" => "命令提示符".into(),
        "explorer" => "资源管理器".into(),
        "notepad" => "记事本".into(),
        "notepad++" => "Notepad++".into(),
        "sublime_text" => "Sublime".into(),
        "winword" => "Word".into(),
        "excel" => "Excel".into(),
        "powerpnt" => "PowerPoint".into(),
        "outlook" => "Outlook".into(),
        "wps" => "WPS".into(),
        "et" => "WPS 表格".into(),
        "wpp" => "WPS 演示".into(),
        "acrobat" | "acrord32" => "Acrobat".into(),
        "foxitreader" | "foxitphantompdf" => "Foxit".into(),
        "applicationframehost" => "应用".into(),
        "wechat" => "微信".into(),
        "wxwork" => "企业微信".into(),
        "feishu" => "飞书".into(),
        "dingtalk" => "钉钉".into(),
        "qq" | "qqnt" => "QQ".into(),
        "telegram" => "Telegram".into(),
        "slack" => "Slack".into(),
        "discord" => "Discord".into(),
        "obsidian" => "Obsidian".into(),
        "typora" => "Typora".into(),
        "screentogif" => "ScreenToGif".into(),
        "sharex" => "ShareX".into(),
        "mstsc" => "远程桌面".into(),
        _ => stem.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::friendly;

    #[test]
    fn known_executables_get_readable_names() {
        assert_eq!(friendly("chrome.exe"), "Chrome");
        assert_eq!(friendly("Code.exe"), "VS Code", "大小写不敏感");
        assert_eq!(friendly("WindowsTerminal.exe"), "终端");
    }

    #[test]
    fn unknown_executables_keep_their_stem() {
        // 去掉扩展名就已经够可读，硬猜一个中文名反而认不出来。
        assert_eq!(friendly("Everything.exe"), "Everything");
        assert_eq!(friendly("weird"), "weird", "没有扩展名也不该崩");
    }

    #[test]
    fn uppercase_extension_is_stripped() {
        assert_eq!(friendly("WINWORD.EXE"), "Word");
        assert_eq!(friendly("Foo.EXE"), "Foo");
    }
}
