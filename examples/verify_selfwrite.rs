//! 验证「写回剪贴板不会把自己造成的变更再记一遍」。
//!
//! 图片写回会同时挂 PNG 和 DIB 两个格式，每次 `SetClipboardData` 都推一个
//! 剪贴板序列号 —— 也就是**一次粘贴会产生两个事件**。早期用一个布尔标志
//! 抑制，只丢得掉第一个，第二个会被当成新复制记进库：粘贴一张图后该条目
//! `hit_count` +1、时间被刷新、位置跳到列表顶部。
//!
//! 现在改成「序列号区间 + 内容指纹」，这个工具量两件事：
//! 1. 自己写回造成的全部事件都被吞掉（hit_count 保持 0）；
//! 2. 区间之外的真实复制仍然照常入库（不能误吞）。
//!
//! 用法：cargo run --release --example verify_selfwrite
//!
//! 注意：会覆盖系统剪贴板内容（最后留一段测试文本）。

use std::time::Duration;

use rpaste::capture::{self, Command, CopyRequest};
use rpaste::model::{Captured, Kind};
use rpaste::store::{InsertOutcome, Store};
use rpaste::win::clipboard;

fn main() {
    let dir = std::env::temp_dir().join(format!("rpaste-selfwrite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut png = Vec::new();
    let img = image::RgbaImage::from_fn(4, 4, |x, y| {
        image::Rgba([(x * 60) as u8, (y * 60) as u8, 200, 255])
    });
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("PNG 编码失败");

    let mut store = Store::open(&dir).expect("建库失败");
    let id = match store
        .insert(&Captured::new(Kind::Image, png.clone(), "图片 4×4".into()))
        .expect("写库失败")
    {
        InsertOutcome::New(id) => id,
        InsertOutcome::Bumped(_) => panic!("首次插入不应命中已有记录"),
    };
    let read = store.read_only().expect("只读连接失败");

    let (tx, rx) = crossbeam_channel::unbounded();
    let (ntx, _nrx) = crossbeam_channel::unbounded();
    std::thread::spawn(move || capture::run(rx, store, ntx));

    // --- 1. 写回图片：应当产生多个 seq，且全部被抑制 ---
    let lo = clipboard::sequence_number();
    tx.send(Command::Copy(CopyRequest {
        kind: Kind::Image,
        payload: png,
        paste: false,
    }))
    .expect("发送 Copy 失败");
    std::thread::sleep(Duration::from_millis(500));

    let hi = clipboard::sequence_number();
    let produced = hi.wrapping_sub(lo);
    println!("写回图片：序列号 ({lo}, {hi}]，产生 {produced} 个事件");
    assert!(produced >= 2, "图片写回应产生至少 2 个事件（PNG + DIB）");

    // 模拟监听线程把每个事件都投给 worker。真实链路里 channel 容量为 1，
    // 事件可能被合并；这里全投是为了压最坏情况。
    let mut seq = lo;
    while seq != hi {
        seq = seq.wrapping_add(1);
        tx.send(Command::Seq(capture::ClipEvent { seq, app: None }))
            .expect("发送 Seq 失败");
    }
    std::thread::sleep(Duration::from_millis(700));

    let row = read
        .recent(10)
        .expect("查询失败")
        .into_iter()
        .find(|e| e.id == id)
        .expect("记录消失了");
    let total = read.total().expect("计数失败");
    println!("自己写回之后：hit_count={} total={total}", row.hit_count);
    assert_eq!(
        row.hit_count, 0,
        "写回被记成了新复制：hit_count 应为 0（旧实现这里会是 1）"
    );
    assert_eq!(total, 1, "写回产生了多余记录");

    // --- 2. 区间之外的真实复制：必须照常入库 ---
    assert!(clipboard::write_text("R-Paste 自激抑制验证 OK"));
    let seq2 = clipboard::sequence_number();
    tx.send(Command::Seq(capture::ClipEvent {
        seq: seq2,
        app: None,
    }))
    .expect("发送 Seq 失败");
    std::thread::sleep(Duration::from_millis(700));

    let total2 = read.total().expect("计数失败");
    println!("外部复制之后：total={total2}");
    assert_eq!(total2, 2, "区间之外的真实复制被误吞了");

    println!(
        "\n全部通过：自己写回的 {} 个事件被抑制，外部复制正常入库。",
        produced
    );
    let _ = tx.send(Command::Shutdown);
    std::thread::sleep(Duration::from_millis(300));
    let _ = std::fs::remove_dir_all(&dir);
}
