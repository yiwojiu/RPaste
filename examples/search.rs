//! 对真实数据库跑搜索，确认内存索引在真实数据上的命中情况。
//!
//! 用法：cargo run --release --example search -- 文件 剪贴板 "数据 超时" wj sjk
//!
//! 中文词走原样子串，纯字母词还会额外走一遍拼音首字母——所以 `wj` 能
//! 命中「文件」，`sjk` 能命中「数据库」。

use std::path::PathBuf;

use rpaste::search::{Index, KindFilter};
use rpaste::store::Store;

fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("RPaste")
}

fn main() {
    let queries: Vec<String> = std::env::args().skip(1).collect();
    let queries = if queries.is_empty() {
        vec![
            "文件".to_string(),
            "剪贴板".to_string(),
            "Cargo".to_string(),
            "wj".to_string(),
            "sjk".to_string(),
        ]
    } else {
        queries
    };

    let store = Store::open(&data_dir()).expect("打不开数据库");
    let read = store.read_only().expect("只读连接失败");

    let t = std::time::Instant::now();
    let all = read.all_meta().expect("读元数据失败");
    let tails = read.all_tails().unwrap_or_default();
    let mut idx = Index::new();
    idx.rebuild(all, &tails);
    println!(
        "库里共 {} 条，读元数据 + 建索引 {:?}",
        idx.len(),
        t.elapsed()
    );

    for q in &queries {
        let t = std::time::Instant::now();
        let hits = idx.query(q, KindFilter::All, 20);
        println!("\n[{q}] {} 条（{:?}）", hits.len(), t.elapsed());
        for h in hits.iter().take(3) {
            let preview: String = h.preview.chars().take(60).collect();
            println!("   #{:<3} {:?}  {}", h.id, h.kind, preview);
        }
    }
}
