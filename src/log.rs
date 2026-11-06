//! 滚动日志。
//!
//! 托盘启动的程序没有控制台，出错时用户什么都看不到，所以必须落盘。
//! 单个文件超过 `MAX_BYTES` 就轮转，最多保留 `KEEP_FILES` 个历史文件，
//! 不会无限增长——剪贴板工具是常驻的，日志不滚动迟早吃满盘。
//!
//! 不往 stderr 写：这个 exe 是 GUI 子系统，没有控制台，stderr 句柄无效。
//! 需要看日志直接读 `%LOCALAPPDATA%\RPaste\rpaste.log`。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 单个日志文件上限 2MB。文本日志一天最多几十 KB，够存很久。
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// 当前文件 + 2 个历史文件。
const KEEP_FILES: usize = 3;

struct Sink {
    path: PathBuf,
    file: File,
    written: u64,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// 初始化日志。失败只会丢日志，不该影响主功能，所以不返回错误。
pub fn init(dir: &Path) {
    let path = dir.join("rpaste.log");
    let (file, written) = match open_append(&path) {
        Some(v) => v,
        None => return,
    };
    let _ = SINK.set(Mutex::new(Sink {
        path,
        file,
        written,
    }));
}

fn open_append(path: &Path) -> Option<(File, u64)> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let size = file.metadata().ok()?.len();
    Some((file, size))
}

impl Sink {
    /// 轮转：删掉最老的，其余依次改名，然后新建空文件。
    fn rotate(&mut self) {
        let base = self.path.display().to_string();

        // 删除超出保留数的那个
        let oldest = format!("{base}.{}", KEEP_FILES - 1);
        let _ = std::fs::remove_file(&oldest);

        // .1 → .2，.2 → .3 …
        for i in (1..KEEP_FILES - 1).rev() {
            let from = format!("{base}.{i}");
            let to = format!("{base}.{}", i + 1);
            if Path::new(&from).exists() {
                let _ = std::fs::rename(&from, &to);
            }
        }

        // 当前 → .1
        let _ = std::fs::rename(&base, format!("{base}.1"));

        if let Some((file, _)) = open_append(&self.path) {
            self.file = file;
            self.written = 0;
        }
    }
}

/// 本地时间戳。
///
/// 不能用 `SystemTime` 直接换算——那是 UTC，日志里写 UTC 而人看的是本地时间，
/// 排查时会白白绕一圈（差 8 小时很容易看漏）。`GetLocalTime` 直接给本地时间。
fn timestamp() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

pub fn write(level: &str, msg: &str) {
    let line = format!("{} [{level}] {msg}\n", timestamp());

    let Some(sink) = SINK.get() else {
        return;
    };
    let Ok(mut sink) = sink.lock() else {
        return;
    };

    if sink.written + line.len() as u64 > MAX_BYTES {
        sink.rotate();
    }
    if sink.file.write_all(line.as_bytes()).is_ok() {
        sink.written += line.len() as u64;
    }
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::write("INFO", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::write("WARN", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::write("ERROR", &format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_local_and_well_formed() {
        let ts = timestamp();
        // 形如 2026-11-04 13:56:24
        assert_eq!(ts.len(), 19, "时间戳格式不对: {ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], " ");
        let year: i32 = ts[..4].parse().expect("年份");
        assert!(year >= 2020, "年份明显不对: {ts}");
    }
}
