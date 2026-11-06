//! 搜索基准：量化 1 万条数据下内存索引的实际表现。
//!
//! 对照数据（同一份数据、同一台机器，改造前用 SQLite FTS5 trigram 测得）：
//!
//! | 场景                                | FTS5 时代 |
//! |-------------------------------------|-----------|
//! | 3 字命中 200 条（带 ORDER BY rank）  | 13–20 ms  |
//! | 3 字命中 200 条（去掉 rank）         | 0.14 ms   |
//! | 0 命中走 FTS                        | 0.09 ms   |
//! | 0 命中走 LIKE 兜底（短词）            | 18 ms     |
//! | 多词「数据 超时」                     | 0 条 ❌    |
//!
//! 用法：cargo run --release --example bench_search

use std::time::Instant;

use rpaste::model::{Captured, Kind, PREVIEW_CHARS, TAIL_CHARS, make_preview_and_tail};
use rpaste::search::{Index, KindFilter};
use rpaste::store::Store;

const N: usize = 10_000;

fn bench(idx: &Index, label: &str, q: &str) {
    let mut best = std::time::Duration::from_secs(999);
    let mut hits = 0;
    for _ in 0..20 {
        let t = Instant::now();
        hits = idx.query(q, KindFilter::All, 200).len();
        best = best.min(t.elapsed());
    }
    println!("{label:<34} {hits:>4} 条  {best:?}");
}

fn main() {
    let dir = std::env::temp_dir().join("rpaste-bench");
    let _ = std::fs::remove_dir_all(&dir);
    let mut store = Store::open(&dir).expect("建库失败");

    let t0 = Instant::now();
    for i in 0..N {
        let text = format!(
            "第{i}条 这是一段用于基准测试的中文文本，包含数据库连接超时、剪贴板监听、\
             性能优化、滚动日志、缩略图缓存等词汇，用来模拟长文本条目在检索时的扫描成本。\
             fn main() {{ println!(\"hello {i}\"); }}"
        );
        let (preview, tail) = make_preview_and_tail(&text, PREVIEW_CHARS, TAIL_CHARS);
        let c = Captured::new(Kind::Text, text.into_bytes(), preview).with_tail(tail);
        store.insert(&c).expect("插入失败");
    }
    store.checkpoint().ok();
    println!(
        "插入 {N} 条：{:?}（带 FTS 触发器时是 11.8 s）",
        t0.elapsed()
    );

    let read = store.read_only().expect("只读连接失败");

    let t = Instant::now();
    let all = read.all_meta().expect("读元数据失败");
    println!("全量读元数据（{} 条）：{:?}", all.len(), t.elapsed());

    let t = Instant::now();
    let tails = read.all_tails().expect("读尾部失败");
    let mut idx = Index::new();
    idx.rebuild(all, &tails);
    println!("建索引（排序 + 拼音键）：{:?}", t.elapsed());

    let weight: usize = idx.list(usize::MAX).iter().map(|e| e.preview.len()).sum();
    println!(
        "索引常驻文本 {:.1} MB（每条 {} 字节）",
        weight as f64 / 1024.0 / 1024.0,
        weight / N
    );

    println!("\n--- 命中很多 ---");
    bench(&idx, "1 字 [数]", "数");
    bench(&idx, "2 字 [数据]", "数据");
    bench(&idx, "3 字 [剪贴板]", "剪贴板");
    bench(&idx, "7 字 [数据库连接超时]", "数据库连接超时");

    println!("\n--- 0 命中（必须扫完 1 万条） ---");
    bench(&idx, "7 字 [不存在的冷门词]", "不存在的冷门词");
    bench(&idx, "6 字英文 [zzqqxx]", "zzqqxx");

    println!("\n--- 多词 / 大小写 ---");
    bench(&idx, "两词 [数据库 超时]", "数据库 超时");
    bench(&idx, "三词 [数据库 超时 剪贴板]", "数据库 超时 剪贴板");
    bench(&idx, "大小写 [SELECT]", "SELECT");
    bench(&idx, "大小写 [HELLO]", "HELLO");

    println!("\n--- 拼音首字母 ---");
    bench(&idx, "两字母 [sjk] → 数据库", "sjk");
    bench(&idx, "三字母 [jtb] → 剪贴板", "jtb");
    bench(&idx, "大写 [SJK]", "SJK");
    bench(&idx, "不可能命中 [zzqqxx]", "zzqqxx");
    bench(&idx, "两词拼音 [sjk cs]", "sjk cs");
    bench(&idx, "混输 [sjk 超时]", "sjk 超时");

    // 长文本的额外成本：每条都带一段尾部时，0 命中查询要多扫这些字节。
    // 现实里只有少数条目超过 512 字，这里按「全库都是长文本」的极端情况压一下。
    println!("\n--- 极端：1 万条各带 2K 字尾部 ---");
    let ids: Vec<i64> = idx.list(usize::MAX).iter().map(|e| e.id).collect();
    let heavy: Vec<(i64, String)> = ids.iter().map(|id| (*id, "尾部内容".repeat(512))).collect();
    let tail_bytes: usize = heavy.iter().map(|(_, t)| t.len()).sum();
    println!(
        "尾部常驻 {:.1} MB（每条 {} 字节）",
        tail_bytes as f64 / 1024.0 / 1024.0,
        tail_bytes / N
    );
    let t = Instant::now();
    idx.rebuild(read.all_meta().expect("读元数据失败"), &heavy);
    println!("重建索引：{:?}", t.elapsed());
    bench(&idx, "0 命中 [不存在的冷门词]", "不存在的冷门词");
    bench(&idx, "尾部命中 [尾部内容]", "尾部内容");

    let _ = std::fs::remove_dir_all(&dir);
}
