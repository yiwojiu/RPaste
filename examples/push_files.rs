//! 把文件列表推到剪贴板（CF_HDROP），用来验证文件捕获。
//!
//! 用法：
//!     cargo run --example push_files -- "D:\a.txt" "D:\b.txt"
//!
//! 这个结构就是 M2「写回文件」用的同一套 `DROPFILES` 布局，所以它同时也是那段
//! 序列化逻辑的活体测试。PowerShell 7 的 Set-Clipboard 没有 -Path 参数，
//! 拿它当测试夹具最省事。

use std::ptr;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

const CF_HDROP: u32 = 15;

/// `DROPFILES` 结构 + 双 NUL 结尾的宽字符路径列表。
fn build_hdrop(paths: &[String]) -> Vec<u8> {
    // DROPFILES 固定 20 字节：pFiles(4) + pt(8) + fNC(4) + fWide(4)
    let mut data: Vec<u8> = Vec::new();
    data.extend_from_slice(&20u32.to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&1i32.to_le_bytes()); // fWide = TRUE

    for p in paths {
        for unit in p.encode_utf16() {
            data.extend_from_slice(&unit.to_le_bytes());
        }
        data.extend_from_slice(&0u16.to_le_bytes());
    }
    // 整个列表以额外一个 NUL 收尾
    data.extend_from_slice(&0u16.to_le_bytes());
    data
}

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("用法: push_files <路径> [路径...]");
        std::process::exit(2);
    }

    let data = build_hdrop(&paths);

    unsafe {
        if OpenClipboard(ptr::null_mut()) == 0 {
            eprintln!("OpenClipboard 失败");
            std::process::exit(1);
        }

        let mem = GlobalAlloc(GMEM_MOVEABLE, data.len());
        if mem.is_null() {
            CloseClipboard();
            eprintln!("GlobalAlloc 失败");
            std::process::exit(1);
        }

        let dst = GlobalLock(mem);
        if dst.is_null() {
            CloseClipboard();
            eprintln!("GlobalLock 失败");
            std::process::exit(1);
        }
        ptr::copy_nonoverlapping(data.as_ptr(), dst as *mut u8, data.len());
        GlobalUnlock(mem);

        EmptyClipboard();
        if SetClipboardData(CF_HDROP, mem as HANDLE).is_null() {
            eprintln!("SetClipboardData 失败");
        }
        CloseClipboard();
    }

    println!("已放入 {} 个文件到剪贴板", paths.len());
}
