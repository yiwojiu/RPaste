//! 量一次「读剪贴板」到底花多久。
//!
//! 用法：先用 `push_image` / 真实的截图工具把内容放进剪贴板，再跑本程序。
//!
//! 它回答的是排查里最常见的一个问题：「图我复制了，为什么没记下来」。
//! 读超时（`capture.rs` 的 `READ_TIMEOUT`）是按这个耗时定的，debug 构建下
//! DIB→PNG 会慢一个数量级，和 release 差了十几倍——同一台机器、同一张图。

fn main() {
    let started = std::time::Instant::now();
    let got = rpaste::win::clipboard::read();
    let elapsed = started.elapsed();

    match got {
        Some(c) => println!(
            "读到 {} · {} · {} 字节，耗时 {elapsed:?}",
            c.kind.name(),
            c.preview.chars().take(40).collect::<String>(),
            c.len()
        ),
        None => println!("读不到内容（空 / 格式不支持 / 被占用），耗时 {elapsed:?}"),
    }
}
