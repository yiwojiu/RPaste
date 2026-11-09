//! 内存检索索引。
//!
//! 为什么不用 SQLite 做检索：
//!
//! 1. **FTS5 的 trigram 分词器只认 3 个字符以上**。中文用户打两个字就想出结果，
//!    于是所有短查询都得退回 `LIKE '%词%'` 全表扫——两条路径的语义还对不齐
//!    （多词在 FTS 里是「短语必须连续」，在 LIKE 里得自己拼 AND）。
//! 2. **`ORDER BY rank` 比扫描还贵**。实测 1 万条：带 rank 13–20 ms，不带 0.14 ms，
//!    而剪贴板根本不需要相关度排序——用户要的是「最近的在前」。
//! 3. 索引表本身占库体积的 1/3，触发器让每次写入多一次维护。
//!
//! 换成内存索引后：1 万条 × 512 字的全量扫描在毫秒级，短词/多词/大小写全都走
//! 同一条代码路径，没有分词器门槛。代价是全量 preview 常驻内存（上限约 15 MB，
//! 实际远小于此，因为多数剪贴板内容很短）。
//!
//! **长文本**（超过 512 字预览上限）会额外带一段「尾部正文」（见
//! `model::TAIL_CHARS`）：预览之外的文字也参与匹配，否则一份 8KB 的脚本
//! 只有开头能搜到，用户复制过的内容等于丢了一半。尾部只存在长条目上，
//! 短条目一条都没有，所以常规路径的内存和速度都不受影响。
//!
//! 尾部带来的成本实测（`examples/bench_search`，1 万条、每条都压满 2K 字尾部、
//! 共 58 MB 的极端情况）：0 命中查询 1.8 ms → 23 ms，尾部命中也只有 50 µs
//! （命中够数就提前退出）。现实里带尾部的条目只有零星几条，这条路径基本看不见。
//!
//! 一致性的兜底很简单：`App::refresh` 每次都拿 `index.len()` 和库里的
//! `COUNT(*)` 比对，对不上就全量重建。任何增量同步的疏漏都会在下一帧自愈。
//!
//! 除了原样子串匹配，还支持**拼音首字母**：输 `wj` 能命中「文件」。做法见
//! `pinyin` 模块——一级汉字在 GB2312 里就是按拼音排序的，所以不需要拼音库。

use std::collections::{HashMap, HashSet};

use crate::model::Kind;
use crate::pinyin;
use crate::store::EntryMeta;

/// 类型筛选。`Text` 把纯文本和富文本合并成一类——用户眼里都是「文字」，
/// 分成两个 chip 只会让人纠结该点哪个。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KindFilter {
    #[default]
    All,
    Text,
    Image,
    Files,
}

impl KindFilter {
    pub fn accepts(self, kind: Kind) -> bool {
        match self {
            Self::All => true,
            Self::Text => matches!(kind, Kind::Text | Kind::RichText),
            Self::Image => kind == Kind::Image,
            Self::Files => kind == Kind::Files,
        }
    }
}

/// 各筛选档位的条数，用来在 chip 上显示计数。
#[derive(Debug, Clone, Copy, Default)]
pub struct Counts {
    pub all: usize,
    pub text: usize,
    pub image: usize,
    pub files: usize,
}

impl Counts {
    pub fn get(self, filter: KindFilter) -> usize {
        match filter {
            KindFilter::All => self.all,
            KindFilter::Text => self.text,
            KindFilter::Image => self.image,
            KindFilter::Files => self.files,
        }
    }
}

/// 索引里的一条记录。
///
/// 把元数据和检索用的「拼音键」绑在一起，而不是维护两个平行数组：`docs` 会
/// 被排序、删除、按 id 合并，平行数组只要有一处忘记同步，键就和条目错位，
/// 表现成「搜出来的东西对不上」。绑在一起就不存在这个问题。
struct Doc {
    meta: EntryMeta,
    /// 拼音首字母键：汉字换成首字母，其余字符原样保留。
    ///
    /// 空串表示这段文本里一个汉字都没有——纯代码、纯路径。这种条目不需要
    /// 拼音路径（原样子串匹配已经覆盖），存空串还能省下几百 KB。
    key: String,
    /// 预览之外的正文尾部（已折叠空白，上限见 `model::TAIL_CHARS`）。
    ///
    /// 空串表示「预览已经是全部内容」——短条目和图片/文件都走这条。长文本
    /// 只有前 512 字能搜是这一版之前的真实缺陷：复制过的内容搜不到，等于历史
    /// 里没有这条记录。
    tail: String,
}

impl Doc {
    fn new(meta: EntryMeta) -> Self {
        let key = initials_key(&meta.preview);
        Self {
            meta,
            key,
            tail: String::new(),
        }
    }

    /// 完整可检索文本：预览 + 尾部。两者在 `model::make_preview_and_tail` 里
    /// 是同一条字符流上切开的，拼回来就是原文（空白已折叠）。
    fn searchable(&self) -> std::borrow::Cow<'_, str> {
        if self.tail.is_empty() {
            std::borrow::Cow::Borrowed(&self.meta.preview)
        } else {
            std::borrow::Cow::Owned(format!("{}{}", self.meta.preview, self.tail))
        }
    }

    /// 单个搜索词是否命中本条。
    ///
    /// 两条路径是**或**关系：
    /// 1. 原样子串（含 ASCII 大小写不敏感），预览和尾部都算；
    /// 2. 拼音首字母。只在「词里含 ASCII 字母」时启用——纯汉字词走原样
    ///    就够，见 `term_key` 的说明。首字母键只覆盖预览（512 字），
    ///    长文本的首字母打在几十 KB 之后本来就是极小概率事件，
    ///    为此把整条尾部再压一遍不划算。
    fn matches(&self, term: &str, term_key: Option<&str>) -> bool {
        if contains_ci(&self.meta.preview, term)
            || (!self.tail.is_empty() && (contains_ci(&self.tail, term) || self.seam_matches(term)))
        {
            return true;
        }
        if self.key.is_empty() {
            return false;
        }
        match term_key {
            // 键两边都已经是小写，可以直接用 `contains`——它走 SIMD 子串
            // 搜索，比逐字节折叠大小写快一倍多。
            Some(k) => self.key.contains(k),
            None => false,
        }
    }

    /// 词正好横跨「预览 / 尾部」的接缝时，两边分开查都查不到。
    ///
    /// 不处理的话就是个**静默漏检**：词在原文里明明连着，用户却搜不到，
    /// 只会得出「这软件搜不全」的结论。接缝只有一处，代价是比对几十个字节，
    /// 所以只在前两条路径都失败之后才走这里。
    fn seam_matches(&self, term: &str) -> bool {
        let head = self.meta.preview.as_bytes();
        let tail = self.tail.as_bytes();
        let needle = term.as_bytes();
        // k = 落在预览末尾的字节数，至少 1 字节，且尾部要放得下剩下的部分。
        let max_k = needle.len().saturating_sub(1).min(head.len());
        (1..=max_k).any(|k| {
            head[head.len() - k..].eq_ignore_ascii_case(&needle[..k])
                && tail.len() >= needle.len() - k
                && tail[..needle.len() - k].eq_ignore_ascii_case(&needle[k..])
        })
    }
}

/// 把文本压成拼音首字母键：汉字 → 首字母，其余字符照抄（ASCII 转小写）。
///
/// 为什么不丢掉非汉字：`abc文件` 压成 `abcwj`，这样「abc」和「wj」两段都能
/// 搜到，也支持「abcwj」这种连着打的中英混输。整串丢掉非汉字的话，
/// 混排内容就只剩下一堆首字母，反而更难命中。
///
/// 一个汉字都没有时返回空串，调用方据此跳过整条拼音路径。
fn initials_key(text: &str) -> String {
    if !text.chars().any(is_han) {
        return String::new();
    }
    initials_key_allow_ascii(text)
}

fn is_han(c: char) -> bool {
    matches!(c, '\u{4E00}'..='\u{9FFF}')
}

/// 同 `initials_key`，但**不做「有没有汉字」的前置判断**。
///
/// 搜索词里的汉字可能全都不在一级字表内（比如生僻的二级字），这时压出来的
/// 串里汉字原样保留，匹配不上是正常的；但如果提前返回空串，就等于把一个
/// 本来还能匹配英文部分的词整个废掉了。
fn initials_key_allow_ascii(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match pinyin::initial(c) {
            Some(initial) => out.push(initial),
            // 二级汉字查不到首字母，原样留着——它匹配不上别人的首字母，
            // 但也不会把前后两段切开。
            None if c.is_ascii_uppercase() => out.push(c.to_ascii_lowercase()),
            None => out.push(c),
        }
    }
    out
}

/// 搜索词自身的拼音键。返回 `None` 表示这个词走不了拼音路径。
///
/// **纯汉字词一律不走拼音**，有两条理由，都不是小事：
///
/// 1. 歧义。「文件」和「我就」的首字母都是 `wj`，压成键之后互相误命中——
///    用户搜「文件」却看到「我就」，比搜不到更让人困惑。
/// 2. 成本。纯汉字词走原样匹配时，绝大多数条目在第一个字节就能被排除；
///    走首字母键则每条都要扫完 257 字节，实测 0 命中从 0.5 ms 涨到 3.7 ms。
///
/// 所以只有含 ASCII 字母的词才启用（`wj`、`abc文件`）：这类词本来就没法
/// 靠原样匹配命中，联想空间才值得付。
fn term_key(term: &str) -> Option<String> {
    if !term.bytes().any(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    if term.chars().any(is_han) {
        // 含汉字的混合词要先压一遍（`abc文件` → `abcwj`），否则永远匹配不上
        // 被压过的文本。
        Some(initials_key_allow_ascii(term))
    } else {
        // 键一律小写，两边形式一致才能用 SIMD 的 `contains`。
        Some(term.to_ascii_lowercase())
    }
}

pub struct Index {
    /// 始终按「收藏在前、其次 created_at 倒序」排列。
    /// 排序只在数据变更时做一次，查询路径上是纯顺序扫描。
    docs: Vec<Doc>,
    /// 已收录的最大 id，增量同步用它问「有没有新条目」。
    max_id: i64,
}

impl Default for Index {
    fn default() -> Self {
        Self::new()
    }
}

impl Index {
    pub fn new() -> Self {
        Self {
            docs: Vec::new(),
            max_id: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    pub fn max_id(&self) -> i64 {
        self.max_id
    }

    /// 全量替换。启动和校正时用。
    ///
    /// 这里给每条算一次拼音键，是最贵的一处：1 万条 × 120 字约 40 ms。
    /// 只在启动和「索引与库对不上」时发生，可以接受。
    ///
    /// `tails` 是 `(id, 尾部正文)` 列表，只包含真正有尾部的条目（库里 `tail`
    /// 非空的行）——短条目占了绝对多数，不需要为它们各查一次。
    pub fn rebuild(&mut self, all: Vec<EntryMeta>, tails: &[(i64, String)]) {
        self.max_id = all.iter().map(|e| e.id).max().unwrap_or(0);
        let mut tail_of: HashMap<i64, &str> =
            tails.iter().map(|(id, t)| (*id, t.as_str())).collect();
        self.docs = all
            .into_iter()
            .map(|e| {
                let mut doc = Doc::new(e);
                if let Some(t) = tail_of.remove(&doc.meta.id) {
                    doc.tail = t.to_owned();
                }
                doc
            })
            .collect();
        self.docs.sort_by(|a, b| ordering(&a.meta, &b.meta));
    }

    /// 合并一批元数据（新条目 + 最近窗口的刷新）和对应的尾部正文。
    ///
    /// 按 id 去重后重排。同一批里出现重复 id 是正常情况——增量查询
    /// （`meta_since`）和最近窗口（`recent`）本来就有交集，后者覆盖前者，
    /// 因为它是同一份数据里更完整的那一份。
    ///
    /// 已有的条目**保留原来的拼音键和尾部**：预览不变，键就不会变，重算一遍
    /// 等于白烧 CPU；尾部没出现在这一批里，说明它没被改动。
    pub fn merge(&mut self, fresh: &[EntryMeta], tails: &[(i64, String)]) {
        if fresh.is_empty() && tails.is_empty() {
            return;
        }
        let mut tail_of: HashMap<i64, String> = tails.iter().cloned().collect();
        let mut by_id: HashMap<i64, Doc> = self.docs.drain(..).map(|d| (d.meta.id, d)).collect();

        for e in fresh {
            let mut doc = by_id.remove(&e.id).unwrap_or_else(|| Doc::new(e.clone()));
            doc.meta = e.clone();
            if let Some(t) = tail_of.remove(&e.id) {
                doc.tail = t;
            }
            by_id.insert(e.id, doc);
        }

        self.docs = by_id.into_values().collect();
        self.docs.sort_by(|a, b| ordering(&a.meta, &b.meta));
        if let Some(max) = fresh.iter().map(|e| e.id).max() {
            self.max_id = self.max_id.max(max);
        }
    }

    /// 按 id 取单条。预览浮层用它——浮层可能显示一条已经滑出
    /// 当前搜索结果范围的条目，不能依赖 `cached_results`。
    pub fn get(&self, id: i64) -> Option<&EntryMeta> {
        self.docs.iter().find(|d| d.meta.id == id).map(|d| &d.meta)
    }

    /// 按当前顺序取前 `limit` 条（收藏优先，其次最近优先）。
    pub fn list(&self, limit: usize) -> Vec<EntryMeta> {
        self.docs
            .iter()
            .take(limit)
            .map(|d| d.meta.clone())
            .collect()
    }

    /// 返回各类型条数。数据变更时算一次，供顶部筛选 chip 显示。
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for d in &self.docs {
            c.all += 1;
            match d.meta.kind {
                Kind::Text | Kind::RichText => c.text += 1,
                Kind::Image => c.image += 1,
                Kind::Files => c.files += 1,
            }
        }
        c
    }

    /// 搜索：空格分隔的多个词是 **AND** 关系，每个词按「子串包含」匹配。
    ///
    /// 每个词同时试两条路径——原样子串，和拼音首字母。两条是**或**关系：
    /// 输 `wj` 既能命中真的含 `wj` 的路径，也能命中「文件」。
    ///
    /// 不做前缀/相关度排序——`docs` 本来就是「收藏在前、其次时间倒序」，
    /// 命中结果保持这个顺序，和列表看到的方向一致，用户不会觉得
    /// 「搜出来的顺序莫名其妙」。
    ///
    /// `filter` 是类型筛选；`q` 为空时等价于「按筛选条件取前 N 条」，
    /// 所以调用方不需要为「有没有搜索词」分两条路径。
    pub fn query(&self, q: &str, filter: KindFilter, limit: usize) -> Vec<EntryMeta> {
        // 词自身的拼音键只算一次，别放进循环里让每条目都重算一遍。
        let terms: Vec<(&str, Option<String>)> =
            q.split_whitespace().map(|t| (t, term_key(t))).collect();

        let mut out = Vec::new();
        for doc in &self.docs {
            if !filter.accepts(doc.meta.kind) {
                continue;
            }
            if terms
                .iter()
                .all(|(raw, key)| doc.matches(raw, key.as_deref()))
            {
                out.push(doc.meta.clone());
                // 命中够数就停：搜索最常见的情况是「很多条都命中」，
                // 提前退出让这条路径的开销和 limit 成正比，而不是和全库成正比。
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }

    /// 给搜索结果生成「命中上下文」，供列表显示。
    ///
    /// 长文本的命中点在 5000 字之后时，列表只画开头的 512 字——用户看到的是一条
    /// 完全不含搜索词的内容，直觉就是「搜索坏了」。这里把命中点前后的一段切出来，
    /// 让人一眼看明白为什么这条被选中。
    ///
    /// 只算 `want` 里的 id，并且由 UI 在**查询变化时调一次**（不是每帧），
    /// 所以这里的逐字符遍历不必省。返回值里只放「和预览不一样」的条目，
    /// 命中本来就在开头的那部分直接复用预览，省一次分配。
    pub fn snippets(
        &self,
        want: &HashSet<i64>,
        terms: &[&str],
        max_chars: usize,
    ) -> HashMap<i64, String> {
        let mut out = HashMap::new();
        if terms.is_empty() || want.is_empty() {
            return out;
        }
        for doc in &self.docs {
            if !want.contains(&doc.meta.id) {
                continue;
            }
            let text = doc.searchable();
            let snippet = hit_context(&text, terms, max_chars);
            if snippet != doc.meta.preview {
                out.insert(doc.meta.id, snippet);
            }
        }
        out
    }

    pub fn remove(&mut self, id: i64) {
        self.docs.retain(|d| d.meta.id != id);
    }

    pub fn retain_pinned(&mut self) {
        self.docs.retain(|d| d.meta.pinned);
    }

    pub fn set_pinned(&mut self, id: i64, pinned: bool) {
        let changed = match self.docs.iter_mut().find(|d| d.meta.id == id) {
            Some(doc) => {
                let changed = doc.meta.pinned != pinned;
                doc.meta.pinned = pinned;
                changed
            }
            None => false,
        };
        // 收藏要「钉在顶部」，所以改完必须重排——只改字段的话条目还蹲在原地，
        // 和用户的直觉对不上。
        if changed {
            self.docs.sort_by(|a, b| ordering(&a.meta, &b.meta));
        }
    }

    /// 仅供测试：确认内部顺序没被写乱。
    #[cfg(test)]
    fn created_ats(&self) -> Vec<i64> {
        self.docs.iter().map(|d| d.meta.created_at).collect()
    }
}

fn ordering(a: &EntryMeta, b: &EntryMeta) -> std::cmp::Ordering {
    // 收藏置顶：「收藏」在所有人的直觉里就是钉住不动，而不只是「免于淘汰」。
    // 同一档内仍按时间倒序，id 兜底保证稳定排序。
    b.pinned
        .cmp(&a.pinned)
        .then(b.created_at.cmp(&a.created_at))
        .then(b.id.cmp(&a.id))
}

/// 从完整文本里切一段包含首个命中的窗口，两端按需加省略号。
///
/// 命中点尽量放在窗口靠前的位置（前 1/3），这样窄窗口里也能看见命中词——
/// 放正中间的话，两行显示只露出前一行，命中反而被挤到看不见的地方。
fn hit_context(text: &str, terms: &[&str], max_chars: usize) -> String {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return text.to_owned();
    }

    let Some(pos) = find_hit(text, terms) else {
        return text.chars().take(max_chars).collect();
    };

    let lead = max_chars / 4;
    let before = text[..pos].chars().count();
    let skip = before.saturating_sub(lead);
    let body: String = text.chars().skip(skip).take(max_chars).collect();

    let mut out = String::with_capacity(body.len() + 8);
    if skip > 0 {
        out.push('…');
    }
    out.push_str(&body);
    if skip + max_chars < total_chars {
        out.push('…');
    }
    out
}

/// 找出 `text` 里所有被命中的字节区间，按起点排序并合并重叠部分。
///
/// 供 UI 做命中高亮用。区间一定落在 UTF-8 边界上：能匹配上说明那一段字节
/// 和查询词逐字节（或忽略 ASCII 大小写后）相等，而 UTF-8 是自同步的，
/// 合法序列不会从字符中间开始。
pub fn match_ranges(text: &str, terms: &[&str]) -> Vec<(usize, usize)> {
    /// 单条文本最多标这么多段。512 字的预览里搜一个单字母理论上能出几百段，
    /// 全都塞进 `LayoutJob` 只会让这一行走形，没有可读性。
    const MAX_RANGES: usize = 128;

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for term in terms {
        if term.is_empty() {
            continue;
        }
        let mut from = 0;
        while from < text.len() && ranges.len() < MAX_RANGES {
            let Some(rel) = find_ci(&text[from..], term) else {
                break;
            };
            let start = from + rel;
            let end = start + term.len();
            ranges.push((start, end));
            // 至少前进一个字节，否则空匹配会死循环（`term` 非空时
            // `start + term.len()` 本身就向前了，这里只是防御）。
            from = end.max(from + 1);
        }
    }

    if ranges.len() < 2 {
        return ranges;
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// 文本里第一个命中任一搜索词的位置（字节偏移）。
///
/// 全文浮层用它决定「从哪儿开始显示」：搜出来一条 8KB 的文本，浮层直接
/// 从头铺开的话，命中的词在几十屏之后，等于又让用户自己找一遍。
pub fn find_hit(text: &str, terms: &[&str]) -> Option<usize> {
    terms
        .iter()
        .filter(|t| !t.is_empty())
        .filter_map(|t| find_ci(text, t))
        .min()
}

/// 子串匹配，返回命中位置（字节偏移）。
///
/// 先用 `str::find` 走快路径：中文和代码片段在绝大多数情况下是原样命中，
/// 它走 SIMD 子串搜索，比逐字节比较快得多。
/// 只有「精确没中，且模式里含 ASCII 字母」才需要退到逐字节折叠比较——
/// 没有字母就不存在大小写变体，精确匹配失败就是最终结论。
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if let Some(p) = hay.find(needle) {
        return Some(p);
    }
    if !needle.bytes().any(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    find_ignore_ascii_case(hay.as_bytes(), needle.as_bytes())
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    find_ci(hay, needle).is_some()
}

fn find_ignore_ascii_case(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    let first = needle[0];
    let last_start = hay.len() - needle.len();
    (0..=last_start).find(|&i| {
        hay[i].eq_ignore_ascii_case(&first) && hay[i..i + needle.len()].eq_ignore_ascii_case(needle)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    fn meta(id: i64, created_at: i64, preview: &str) -> EntryMeta {
        EntryMeta {
            id,
            kind: Kind::Text,
            preview: preview.to_owned(),
            byte_len: preview.len() as i64,
            created_at,
            hit_count: 0,
            pinned: false,
            app: String::new(),
        }
    }

    fn index_of(items: Vec<EntryMeta>) -> Index {
        let mut idx = Index::new();
        idx.rebuild(items, &[]);
        idx
    }

    #[test]
    fn results_are_newest_first() {
        let idx = index_of(vec![
            meta(1, 100, "a"),
            meta(3, 300, "c"),
            meta(2, 200, "b"),
        ]);
        let ids: Vec<i64> = idx.list(10).iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![3, 2, 1]);
    }

    #[test]
    fn single_chinese_char_matches() {
        // 这条在 FTS5 trigram 下永远 0 命中（<3 字符不产生 token）。
        let idx = index_of(vec![meta(1, 100, "复制文件到剪贴板")]);
        assert_eq!(idx.query("件", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("剪贴", KindFilter::All, 10).len(), 1);
    }

    #[test]
    fn multiple_terms_are_anded() {
        let idx = index_of(vec![
            meta(1, 100, "数据库连接超时"),
            meta(2, 90, "数据库备份"),
        ]);
        // 两词在原文中不相邻，旧的短语查询在这里会 0 命中。
        assert_eq!(idx.query("数据库 超时", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("数据库 备份", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("数据库 不存在", KindFilter::All, 10).len(), 0);
    }

    #[test]
    fn ascii_case_is_ignored() {
        let idx = index_of(vec![meta(1, 100, "SELECT * FROM Clips")]);
        assert_eq!(idx.query("select", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("SELECT", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("from clips", KindFilter::All, 10).len(), 1);
    }

    #[test]
    fn merge_updates_order_after_bump() {
        let mut idx = index_of(vec![meta(1, 100, "old"), meta(2, 200, "mid")]);
        // id=1 被再次复制：时间戳变最新，顺序要跟着变。
        idx.merge(&[meta(1, 300, "old")], &[]);
        let ids: Vec<i64> = idx.list(10).iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 2]);
        assert_eq!(idx.len(), 2, "合并不能产生重复条目");
    }

    #[test]
    fn merge_adds_new_entry_and_tracks_max_id() {
        let mut idx = index_of(vec![meta(1, 100, "a")]);
        idx.merge(&[meta(7, 400, "b")], &[]);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.max_id(), 7);
        assert_eq!(idx.list(10)[0].id, 7);
    }

    #[test]
    fn empty_query_lists_everything() {
        let idx = index_of(vec![meta(1, 100, "a"), meta(2, 200, "b")]);
        assert_eq!(idx.query("   ", KindFilter::All, 10).len(), 2);
    }

    #[test]
    fn limit_respected() {
        let items = (0..50).map(|i| meta(i, i, "命中")).collect();
        let idx = index_of(items);
        assert_eq!(idx.query("命中", KindFilter::All, 5).len(), 5);
        assert_eq!(idx.list(3).len(), 3);
    }

    #[test]
    fn wildcards_are_literal() {
        let idx = index_of(vec![
            meta(1, 100, "progress 100% done"),
            meta(2, 90, "other entry"),
        ]);
        // SQLite 的 LIKE 里 % 是通配符，得转义；内存子串匹配没有这个概念，
        // 用户输什么就找什么。
        assert_eq!(idx.query("%", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("_", KindFilter::All, 10).len(), 0);
    }

    #[test]
    fn created_ats_stay_descending() {
        let mut idx = index_of(vec![
            meta(1, 300, "a"),
            meta(2, 100, "b"),
            meta(3, 200, "c"),
        ]);
        idx.merge(&[meta(2, 900, "b")], &[]);
        let ts = idx.created_ats();
        assert!(ts.windows(2).all(|w| w[0] >= w[1]), "{ts:?} 不是倒序");
    }

    fn meta_kind(id: i64, created_at: i64, preview: &str, kind: Kind) -> EntryMeta {
        EntryMeta {
            kind,
            ..meta(id, created_at, preview)
        }
    }

    #[test]
    fn pinned_items_float_to_top() {
        let mut idx = index_of(vec![
            meta(1, 300, "最新"),
            meta(2, 200, "中间"),
            meta(3, 100, "最旧"),
        ]);
        idx.set_pinned(3, true);

        let ids: Vec<i64> = idx.list(10).iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![3, 1, 2], "收藏项要顶到最前，其余仍按时间倒序");

        // 取消收藏后要回到原来的时间位置，不能「钉上去就下不来」。
        idx.set_pinned(3, false);
        let ids: Vec<i64> = idx.list(10).iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn pinned_ordering_survives_merge() {
        let mut idx = index_of(vec![meta(1, 300, "a"), meta(2, 200, "b")]);
        idx.set_pinned(1, true);
        // 新条目进来会触发重排，收藏位不能丢。
        idx.merge(&[meta(9, 900, "new")], &[]);
        let ids: Vec<i64> = idx.list(10).iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 9, 2]);
    }

    #[test]
    fn filter_narrows_by_kind() {
        let idx = index_of(vec![
            meta_kind(1, 400, "文本条目", Kind::Text),
            meta_kind(2, 300, "富文本条目", Kind::RichText),
            meta_kind(3, 200, "图片 800×600", Kind::Image),
            meta_kind(4, 100, "C:\\a.txt", Kind::Files),
        ]);

        let count = |f: KindFilter| idx.query("", f, 10).len();
        assert_eq!(count(KindFilter::All), 4);
        // 纯文本和富文本算同一类，用户眼里都是「文字」。
        assert_eq!(count(KindFilter::Text), 2);
        assert_eq!(count(KindFilter::Image), 1);
        assert_eq!(count(KindFilter::Files), 1);

        // 筛选和搜索词是叠加关系，不是二选一。
        assert_eq!(idx.query("条目", KindFilter::Image, 10).len(), 0);
        assert_eq!(idx.query("条目", KindFilter::Text, 10).len(), 2);
    }

    #[test]
    fn counts_match_filter_results() {
        let idx = index_of(vec![
            meta_kind(1, 400, "a", Kind::Text),
            meta_kind(2, 300, "b", Kind::RichText),
            meta_kind(3, 200, "c", Kind::Image),
            meta_kind(4, 100, "d", Kind::Files),
            meta_kind(5, 50, "e", Kind::Files),
        ]);
        let c = idx.counts();
        assert_eq!((c.all, c.text, c.image, c.files), (5, 2, 1, 2));
        for f in [
            KindFilter::All,
            KindFilter::Text,
            KindFilter::Image,
            KindFilter::Files,
        ] {
            assert_eq!(c.get(f), idx.query("", f, 100).len(), "{f:?} 计数对不上");
        }
    }

    #[test]
    fn match_ranges_marks_every_occurrence() {
        let text = "abc 中文 abc";
        let r = match_ranges(text, &["abc"]);
        assert_eq!(r, vec![(0, 3), (11, 14)]);

        // 多个词各标各的，重叠部分合并成一段（「数据」0..6、「据库」3..9）。
        let r = match_ranges("数据库连接超时", &["数据", "据库"]);
        assert_eq!(r, vec![(0, 9)], "重叠区间要合并");
    }

    #[test]
    fn match_ranges_is_case_insensitive_and_boundary_safe() {
        let text = "SELECT * FROM Clips";
        assert_eq!(match_ranges(text, &["select"]), vec![(0, 6)]);

        // 区间必须落在字符边界上，否则按它切片会 panic。
        let zh = "中文命中测试";
        for (s, e) in match_ranges(zh, &["命中"]) {
            assert!(zh.is_char_boundary(s) && zh.is_char_boundary(e), "{s}..{e}");
        }
        // 空词不该产生任何区间——否则整行都被染色。
        assert!(match_ranges("abc", &[""]).is_empty());
    }

    #[test]
    fn pinyin_initials_find_chinese_entries() {
        let idx = index_of(vec![
            meta(1, 300, "复制文件到剪贴板"),
            meta(2, 200, "SELECT * FROM clips"),
            meta(3, 100, "数据库连接超时"),
        ]);

        // 首字母：文件 → wj。这条是「搜不到」问题的正解——原文里根本没有
        // 字母 w 和 j 连在一起，只有走进拼音路径才能命中。
        assert_eq!(idx.query("wj", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("sjk", KindFilter::All, 10).len(), 1);
        // 多个单词各自走拼音，AND 关系不变。
        assert_eq!(idx.query("wj jtb", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("wj sjk", KindFilter::All, 10).len(), 0);
    }

    #[test]
    fn pinyin_is_case_insensitive() {
        let idx = index_of(vec![meta(1, 100, "数据库连接超时")]);
        assert_eq!(idx.query("SJK", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("Sjk", KindFilter::All, 10).len(), 1);
    }

    #[test]
    fn literal_ascii_match_still_wins() {
        // 拼音路径是「或」不是「替换」：真的含 wj 的条目也得命中，
        // 不能因为它是 ASCII 就被拿去按首字母解释。
        let idx = index_of(vec![meta(1, 100, "D:\\proj\\wj.txt")]);
        assert_eq!(idx.query("wj", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("proj", KindFilter::All, 10).len(), 1);
    }

    #[test]
    fn mixed_chinese_and_ascii_uses_one_key() {
        let idx = index_of(vec![meta(1, 100, "abc文件已保存")]);
        // 文本压成 `abcwjysb`：英文段和首字母段在同一根串上，
        // 所以分开搜、连着打都能命中。
        assert_eq!(idx.query("abc", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("wj", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("abcwj", KindFilter::All, 10).len(), 1);
        assert_eq!(
            idx.query("wjs", KindFilter::All, 10).len(),
            0,
            "跨词不该拼起来"
        );
    }

    #[test]
    fn pure_chinese_query_does_not_go_through_pinyin() {
        let idx = index_of(vec![meta(1, 200, "我就随便说说"), meta(2, 100, "文件名")]);
        // 「文件」和「我就」的首字母都是 wj。汉字词必须只走原样匹配，
        // 否则搜「文件」会把「我就」一起带出来——比搜不到更让人困惑。
        let hits = idx.query("文件", KindFilter::All, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 2);
        assert_eq!(term_key("文件"), None);
        assert_eq!(term_key("123"), None);
        assert_eq!(term_key("wj").as_deref(), Some("wj"));
    }

    #[test]
    fn pure_ascii_text_has_no_pinyin_path() {
        // 「Hello World」的首字母是 hw，但纯英文条目不该被首字母解释——
        // 那样所有含 h/w 的词都会乱命中。键留空，只走原样匹配。
        let idx = index_of(vec![meta(1, 100, "Hello World")]);
        assert_eq!(idx.query("hw", KindFilter::All, 10).len(), 0);
        assert_eq!(idx.query("hello", KindFilter::All, 10).len(), 1);
    }

    #[test]
    fn initials_key_skips_text_without_hanzi() {
        assert_eq!(initials_key("docker run -it"), "");
        assert_eq!(initials_key(""), "");
        assert_eq!(initials_key("文件"), "wj");
        assert_eq!(initials_key("abc文件"), "abcwj");
        // 标点保留原样，不会把前后两段粘成新的词。
        assert_eq!(initials_key("文件, 备份"), "wj, bf");
    }

    #[test]
    fn query_filters_still_apply_with_pinyin() {
        let idx = index_of(vec![
            meta_kind(1, 300, "文件快照 800×600", Kind::Image),
            meta_kind(2, 200, "文件路径", Kind::Files),
        ]);
        assert_eq!(idx.query("wj", KindFilter::All, 10).len(), 2);
        assert_eq!(idx.query("wj", KindFilter::Image, 10).len(), 1);
        assert_eq!(idx.query("wj", KindFilter::Text, 10).len(), 0);
    }

    /// 造一条「预览 512 字 + 尾部」的长条目，模拟真实库里 >512 字的内容。
    fn long_index(preview: &str, tail: &str) -> Index {
        let mut idx = Index::new();
        idx.rebuild(vec![meta(1, 100, preview)], &[(1, tail.to_owned())]);
        idx
    }

    #[test]
    fn text_beyond_preview_is_searchable() {
        let preview = "开".repeat(512);
        let tail = format!("{}关键结论在这里", "中间内容".repeat(600));
        let idx = long_index(&preview, &tail);

        // 这条就是用户报的「长文本搜不到」：词在 5000 字之后，
        // 旧版本只索引前 512 字，这里必然 0 命中。
        assert_eq!(idx.query("关键结论", KindFilter::All, 10).len(), 1);
        // 预览范围内的内容照样命中。
        assert_eq!(idx.query("开开", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("完全没有的词", KindFilter::All, 10).len(), 0);
    }

    #[test]
    fn term_straddling_preview_and_tail_matches() {
        // 词正好压在接缝上：预览以 "abc" 结尾，尾部以 "def" 开头。
        // 分开查两边都查不到，拼接之后才存在——不处理就是静默漏检。
        let preview = format!("{}abc", "前".repeat(512));
        let idx = long_index(&preview, "defghi");
        assert_eq!(idx.query("abcdef", KindFilter::All, 10).len(), 1);
        assert_eq!(idx.query("cde", KindFilter::All, 10).len(), 1);
        // 只是「各自都在、但并不相邻」的组合不能算命中。
        assert_eq!(idx.query("abxef", KindFilter::All, 10).len(), 0);
    }

    #[test]
    fn merge_keeps_tail_of_existing_entries() {
        // 刷新时只带新条目的尾部，老条目的尾部不能因此丢掉。
        let mut idx = long_index(&"开".repeat(512), "后面的关键词");
        idx.merge(&[meta(2, 200, "新条目")], &[]);
        assert_eq!(idx.query("关键词", KindFilter::All, 10).len(), 1);
    }

    #[test]
    fn snippet_centers_on_late_hits() {
        let preview = "开".repeat(512);
        let tail = format!("{}命中位置", "中间".repeat(1500));
        let idx = long_index(&preview, &tail);

        let ids: HashSet<i64> = [1].into_iter().collect();
        let snips = idx.snippets(&ids, &["命中位置"], 200);
        let snippet = snips.get(&1).expect("命中在尾部，必须给出上下文");
        assert!(snippet.contains("命中位置"), "{snippet}");
        assert!(snippet.starts_with('…'), "前面还有一大段，得让人知道");
        assert!(snippet.chars().count() <= 202, "窗口要受控");
    }

    #[test]
    fn snippet_skipped_when_hit_is_at_the_top() {
        // 命中就在开头：直接复用预览，不必再切一段。
        let idx = index_of(vec![meta(1, 100, "开头就命中的短文本")]);
        let ids: HashSet<i64> = [1].into_iter().collect();
        assert!(idx.snippets(&ids, &["开头"], 200).is_empty());
    }
}
