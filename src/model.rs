//! 平台无关的条目模型。
//!
//! 这一层刻意不依赖 `windows-sys`：将来加 macOS / Wayland 时只补平台模块，
//! 模型、存储、检索都不需要动。

/// 小于该长度的内容直接内联进 SQLite 页内，不产生额外文件 IO。
pub const INLINE_LIMIT: usize = 64 * 1024;

/// 单条内容的大小上限。超过直接丢弃，防止一个 200MB 的截图把内存打满。
pub const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

/// 预览截断的字符数（不是字节数，中文按字算）。
pub const PREVIEW_CHARS: usize = 512;

/// 预览之后还要额外留多少字符给检索（正文尾部）。
///
/// 512 字对代码片段听着够，对真实内容远远不够：一段 8KB 的 SQL 脚本、一份配置、
/// 一条长日志，只有前 512 字能被搜到，后面等于不存在——用户复制过的东西搜不出来，
/// 这是最伤信任的一类 bug。
///
/// 尾部单独存一列（不塞进 `preview`），因为 `EntryMeta` 会在 UI 里被大量克隆，
/// 每条带上十几 KB 的正文会把每帧的复制成本抬上来。检索时两者拼起来一起匹配。
///
/// 16KB 是取舍：一份「刚复制的那段」的典型上限。1 万条全是超长文本文本时
/// 内存占用约 160MB——那意味着库里有 160MB 的剪贴板文本，本身已经不正常了。
pub const TAIL_CHARS: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum Kind {
    Text = 0,
    RichText = 1,
    Image = 2,
    Files = 3,
}

impl Kind {
    pub fn as_i64(self) -> i64 {
        self as i64
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::RichText => "rich",
            Kind::Image => "image",
            Kind::Files => "files",
        }
    }

    pub fn from_i64(v: i64) -> Option<Self> {
        match v {
            0 => Some(Kind::Text),
            1 => Some(Kind::RichText),
            2 => Some(Kind::Image),
            3 => Some(Kind::Files),
            _ => None,
        }
    }
}

/// 一次捕获到的剪贴板内容，已经过归一化。
pub struct Captured {
    pub kind: Kind,
    /// blake3(payload)。归一化之后算，所以同一个内容的 `\r\n` / `\n` 差异不会产生两条记录。
    pub hash: [u8; 32],
    pub payload: Vec<u8>,
    pub preview: String,
    /// 预览之外的正文尾部，只用于检索。没有尾部时是空串。
    pub tail: String,
}

impl Captured {
    pub fn new(kind: Kind, payload: Vec<u8>, preview: String) -> Self {
        let hash = *blake3::hash(&payload).as_bytes();
        Self {
            kind,
            hash,
            payload,
            preview,
            tail: String::new(),
        }
    }

    /// 带上检索用的正文尾部。文本之外的类型不需要（预览已经覆盖了全部信息）。
    pub fn with_tail(mut self, tail: String) -> Self {
        self.tail = tail;
        self
    }

    pub fn len(&self) -> usize {
        self.payload.len()
    }

    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

/// 生成单行预览：折叠所有空白、截断到 `max_chars` 个字符。
///
/// 不按字节截断，否则会把中文或 emoji 切成半个码点。
pub fn make_preview(text: &str, max_chars: usize) -> String {
    make_preview_and_tail(text, max_chars, 0).0
}

/// 一次遍历同时产出「列表预览」和「检索用的正文尾部」。
///
/// 两者接在折叠空白之后同一条字符流上切分，所以拼起来正好是原文（去掉多余空白）：
/// 预览取前 `preview_chars` 个字，尾部从它后面接着取 `tail_chars` 个字。
/// 尾部起点按**未 trim 的头部**算，这样预览末尾被 trim 掉的空白自然变成两者之间的
/// 分隔符，拼起来不会把两个词粘成一个。
pub fn make_preview_and_tail(
    text: &str,
    preview_chars: usize,
    tail_chars: usize,
) -> (String, String) {
    let collapsed = collapse_whitespace(text, preview_chars.saturating_add(tail_chars));
    let head: String = collapsed.chars().take(preview_chars).collect();

    // 预览要去掉首尾空白，但尾部必须从「预览本体之后」接着取，而不是从 head
    // 之后取：head 末尾被 trim 掉的那个空格属于两者的接缝，留在尾部开头，
    // 拼起来才是 "第一句 第二句" 而不是 "第一句第二句"。
    let body = head.trim_start();
    let preview = body.trim_end();
    let skip = head.len() - body.len() + preview.len();
    let tail = collapsed.get(skip..).unwrap_or_default().to_owned();

    (preview.to_owned(), tail)
}

/// 把所有空白压成单个空格，最多产出 `max_chars` 个字符。
///
/// 折叠是必要的：剪贴板里到处是缩进和空行，不压的话预览第一行往往是空的，
/// 而且「跨行的两个词」用空格搜永远搜不到。
fn collapse_whitespace(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(max_chars.min(text.len()));
    let mut n = 0usize;
    let mut last_ws = false;

    for c in text.chars() {
        if c.is_whitespace() {
            if last_ws {
                continue;
            }
            last_ws = true;
            out.push(' ');
        } else {
            last_ws = false;
            out.push(c);
        }
        n += 1;
        if n >= max_chars {
            break;
        }
    }

    out
}

/// 把 CRLF 归一成 LF。Windows 剪贴板里两者混用很常见，
/// 不归一的话同一段文字从不同程序复制会算成两个不同的条目。
pub fn normalize_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// 写回剪贴板前反向转换：Windows 上的记事本 / Office 更认 CRLF。
pub fn denormalize_newlines(s: &str) -> String {
    if !s.contains('\n') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 64);
    for c in s.chars() {
        if c == '\n' {
            out.push('\r');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_collapses_whitespace() {
        assert_eq!(make_preview("  hello \n\n  world  ", 100), "hello world");
    }

    #[test]
    fn preview_truncates_on_char_boundary() {
        let s = "中文中文中文中文";
        assert_eq!(make_preview(s, 3), "中文中");
    }

    #[test]
    fn preview_and_tail_cover_the_whole_text() {
        let s = "abcdefghij";
        let (preview, tail) = make_preview_and_tail(s, 4, 100);
        assert_eq!(preview, "abcd");
        assert_eq!(tail, "efghij", "尾部要接着预览往下取，不能从头再来");
        // 拼接回去就是原文：检索路径依赖这一点。
        assert_eq!(format!("{preview}{tail}"), s);
    }

    #[test]
    fn tail_is_capped_and_empty_for_short_text() {
        let s = "a".repeat(100);
        let (_, tail) = make_preview_and_tail(&s, 10, 20);
        assert_eq!(tail.chars().count(), 20);

        // 短文本没有尾部——空串，不是 NULL，检索时直接跳过。
        let (_, tail) = make_preview_and_tail("短", 512, 20);
        assert_eq!(tail, "");
    }

    #[test]
    fn tail_survives_whitespace_collapse() {
        // 预览末尾的换行被 trim 掉，但尾部从 trim 之前的位置开始，
        // 所以拼起来仍然是一个空格分隔，不会变成 "前一句后一句"。
        let s = "第一句\n\n   第二句";
        let (preview, tail) = make_preview_and_tail(s, 4, 100);
        assert_eq!(preview, "第一句", "预览首尾空白要剪掉");
        assert_eq!(format!("{preview}{tail}"), "第一句 第二句");
        assert!(tail.contains("第二句"), "跨行的内容必须在同一根串上");
    }

    #[test]
    fn normalize_handles_mixed_endings() {
        assert_eq!(normalize_newlines("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(normalize_newlines("no newline"), "no newline");
    }

    #[test]
    fn denormalize_is_inverse() {
        let s = "a\nb\nc";
        assert_eq!(normalize_newlines(&denormalize_newlines(s)), s);
    }
}
