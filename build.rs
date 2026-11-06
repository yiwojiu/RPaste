//! 把图标和版本信息编进 exe 的资源节。
//!
//! 资源编译要靠 MSVC 的 `rc.exe`，它在 Windows SDK 里。默认情况下 winresource
//! 会去翻注册表找 SDK 安装路径，但本机的安全策略禁止启动 `reg.exe`，
//! 所以这里改成自己扫 SDK 目录、把结果直接喂给它。
//!
//! 这一步失败**不能让构建挂掉**：exe 没有文件图标照样能跑，托盘和窗口图标
//! 走的是另一条路（直接解析 ICO 字节），不依赖资源节。失败只打 warning。
//!
//! 字符串用英文：`.rc` 默认按系统 ANSI 码页解析，写中文在部分区域设置下会乱码，
//! 而这几项只是文件属性里扫一眼的东西，不值得冒这个险。

use std::path::{Path, PathBuf};

/// 在 Windows SDK 目录里找 `rc.exe`，多个版本取最高的那个。
///
/// 不查注册表：`reg.exe` 被本机安全策略拦了，调用会直接失败。
fn find_rc() -> Option<PathBuf> {
    let base = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut best: Option<(Vec<u32>, PathBuf)> = None;

    for entry in std::fs::read_dir(base).ok()?.flatten() {
        let dir = entry.path();
        // 只认 10.0.xxxxx.0 这种版本目录，里面有 x64/rc.exe 才算数
        let rc = dir.join("x64").join("rc.exe");
        if !rc.exists() {
            continue;
        }
        let ver: Vec<u32> = dir
            .file_name()
            .map(|n| {
                n.to_string_lossy()
                    .split('.')
                    .filter_map(|s| s.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        if best.as_ref().is_none_or(|(v, _)| ver > *v) {
            best = Some((ver, rc));
        }
    }

    best.map(|(_, p)| p)
}

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set("ProductName", "R-Paste");
    res.set("FileDescription", "R-Paste Clipboard History");
    res.set("OriginalFilename", "rpaste.exe");

    match find_rc() {
        Some(rc) => {
            if let Some(dir) = rc.parent().and_then(|d| d.to_str()) {
                res.set_toolkit_path(dir);
            }
        }
        None => {
            println!("cargo:warning=没找到 rc.exe，exe 将不显示图标");
            return;
        }
    }

    if let Err(e) = res.compile() {
        println!("cargo:warning=资源编译失败，exe 将不显示图标: {e}");
    }
}
