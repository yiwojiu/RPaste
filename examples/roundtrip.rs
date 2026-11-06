//! 端到端验证写回：从数据库取一条历史 → 写回剪贴板 → 再读回来 → 比对哈希。
//!
//! 用法：
//!     cargo run --example roundtrip -- 21        # 图片条目
//!     cargo run --example roundtrip -- 17 files  # 文件条目
//!     cargo run --example roundtrip -- 16 text   # 文本条目

use std::path::PathBuf;
use std::time::Duration;

use rpaste::model::Kind;
use rpaste::store::Store;
use rpaste::win::clipboard;

fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("RPaste")
}

fn main() {
    let mut args = std::env::args().skip(1);
    let id: i64 = args.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| {
        eprintln!("用法: roundtrip <条目 id> [kind]");
        std::process::exit(2);
    });

    let root = data_dir();
    let store = Store::open(&root).expect("打不开数据库");
    let read = store.read_only().expect("只读连接失败");

    let payload = read.load_payload(id).expect("查询失败").unwrap_or_else(|| {
        eprintln!("条目 {id} 不存在或没有内容");
        std::process::exit(1);
    });

    let meta = read.recent(500).unwrap_or_default();
    let kind = meta
        .iter()
        .find(|e| e.id == id)
        .map(|e| e.kind)
        .unwrap_or(Kind::Text);

    println!("源条目 #{id} kind={:?} {} 字节", kind, payload.len());
    let src_hash = blake3::hash(&payload);

    let written = match kind {
        Kind::Text => {
            let text = String::from_utf8_lossy(&payload).into_owned();
            clipboard::write_text(&text)
        }
        Kind::Image => clipboard::write_image(&payload),
        Kind::Files => {
            let text = String::from_utf8_lossy(&payload);
            let paths: Vec<String> = text.lines().map(|s| s.to_owned()).collect();
            clipboard::write_files(&paths)
        }
        Kind::RichText => clipboard::write_text(&String::from_utf8_lossy(&payload)),
    };
    println!("write -> {written}");
    if !written {
        std::process::exit(1);
    }

    // 给剪贴板一点时间落地，延迟渲染的接收方需要时间。
    std::thread::sleep(Duration::from_millis(300));

    match clipboard::read() {
        Some(back) => {
            println!("读回 kind={:?} {} 字节", back.kind, back.len());
            let back_hash = blake3::hash(&back.payload);
            if src_hash == back_hash {
                println!("hash 完全一致 ✓");
            } else if back.kind == kind {
                println!("hash 不同（可能经过了格式转换），类型一致");
            } else {
                println!("hash 和类型都不一致 ✗  {}", src_hash.to_hex());
                std::process::exit(1);
            }
        }
        None => {
            println!("读回失败：剪贴板无可捕获内容 ✗");
            std::process::exit(1);
        }
    }
}
