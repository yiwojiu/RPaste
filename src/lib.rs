//! R-Paste 核心库。
//!
//! 拆成 lib + bin 是为了让 `examples/` 和集成测试能直接复用捕获、存储、写回逻辑，
//! 不用在测试工具里重复实现一遍 Win32 细节。

pub mod capture;
pub mod log;
pub mod model;
pub mod pinyin;
pub mod search;
pub mod store;
pub mod theme;
pub mod thumb;
pub mod ui;
pub mod win;
