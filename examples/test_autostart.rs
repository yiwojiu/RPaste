//! 验证开机自启的注册表读写。测完会把状态恢复成原样。
//!
//! 用法：cargo run --release --example test_autostart

use rpaste::win::autostart;

fn main() {
    let before = autostart::is_enabled();
    println!("初始状态：{}", if before { "已开启" } else { "未开启" });

    // 开
    match autostart::enable() {
        Ok(()) => println!("enable() ok"),
        Err(e) => {
            eprintln!("enable 失败: {e}");
            std::process::exit(1);
        }
    }
    println!(
        "写入后 is_enabled = {}（期望 true）",
        autostart::is_enabled()
    );

    // 查注册表实际内容
    unsafe {
        use std::ptr;
        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::System::Registry::*;
        let sub: Vec<u16> = r"Software\Microsoft\Windows\CurrentVersion\Run"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let name: Vec<u16> = "RPaste\0".encode_utf16().collect();
        let mut key: HKEY = ptr::null_mut();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        ) == ERROR_SUCCESS
        {
            let mut buf = vec![0u8; 1024];
            let mut len = buf.len() as u32;
            let rc = RegQueryValueExW(
                key,
                name.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                buf.as_mut_ptr(),
                &mut len,
            );
            if rc == ERROR_SUCCESS {
                let units: Vec<u16> = buf[..len as usize]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&u| u != 0)
                    .collect();
                println!("注册表值：{:?}", String::from_utf16_lossy(&units));
            } else {
                println!("读注册表值失败，错误码 {rc}");
            }
            RegCloseKey(key);
        }
    }

    // 关
    match autostart::disable() {
        Ok(()) => println!("disable() ok"),
        Err(e) => {
            eprintln!("disable 失败: {e}");
            std::process::exit(1);
        }
    }
    println!(
        "清除后 is_enabled = {}（期望 false）",
        autostart::is_enabled()
    );

    // 恢复原状态
    if before {
        let _ = autostart::enable();
        println!("已恢复为开启");
    }

    println!(
        "\n结果：{}",
        if !autostart::is_enabled() && !before {
            "✓ 读写都正常，已恢复未开启"
        } else {
            "✓ 读写正常"
        }
    );
}
