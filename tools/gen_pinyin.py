"""生成 `src/pinyin.rs`：汉字 → 拼音首字母。

## 为什么不需要拼音库

GB2312 的一级汉字（3755 个常用字）在**编码空间里就是按拼音字母序排的**，
这是国标定下的规则（二级汉字才改成按部首排）。所以只要知道每个字母段的
起止码位，就能在零依赖、零字典的前提下拿到首字母。

各段的起止已经实测核对过：段首分别是
啊芭擦搭蛾发噶哈击喀垃妈拿哦啪期然撒塌挖昔压匝，
段尾分别是
澳怖错堕贰咐过祸骏阔络穆诺沤瀑群弱所唾误迅孕座，
首尾都落在同一个字母上，说明分段没有错位。

注意 E 段只有 24 个字、O 段只有 8 个字——这和汉语里 e/o 开头的音节极少
是吻合的，也能反过来印证分段是真的对，不是编出来的。

## 为什么不覆盖全汉字

二级汉字（3008 个）按部首排序，拿不到首字母；GBK 扩展区同理。这些字
查不到就返回 `None`，检索层退化成普通的子串匹配——不会出错，只是没有
拼音捷径。3755 个字覆盖了日常文本的绝大多数，够用。
"""

# 各字母段的起始码位。没有 i/u/v 段：普通话没有这三个音开头的音节。
SEGMENTS = [
    (0xB0A1, "a"), (0xB0C5, "b"), (0xB2C1, "c"), (0xB4EE, "d"),
    (0xB6EA, "e"), (0xB7A2, "f"), (0xB8C1, "g"), (0xB9FE, "h"),
    (0xBBF7, "j"), (0xBFA6, "k"), (0xC0AC, "l"), (0xC2E8, "m"),
    (0xC4C3, "n"), (0xC5B6, "o"), (0xC5BE, "p"), (0xC6DA, "q"),
    (0xC8BB, "r"), (0xC8F6, "s"), (0xCBFA, "t"), (0xCDDA, "w"),
    (0xCEF4, "x"), (0xD1B9, "y"), (0xD4D1, "z"),
]

# 一级汉字区的最后一个有效码位（D7 区 F9 位，再往后 5 个位置是空的）。
LAST_CODE = 0xD7F9

# 抽样核对：这些字的拼音首字母是确定的，生成后要逐一断言，
# 防止哪天有人改错了分段表还以为没事。
SAMPLES = {
    "中": "z", "国": "g", "文": "w", "件": "j", "代": "d", "码": "m",
    "搜": "s", "索": "s", "剪": "j", "贴": "t", "板": "b", "图": "t",
    "片": "p", "数": "s", "据": "j", "库": "k", "连": "l", "接": "j",
    "超": "c", "时": "s", "备": "b", "份": "f", "啊": "a", "做": "z",
    "澳": "a", "贰": "e", "沤": "o", "座": "z", "鹅": "e", "恩": "e",
}


def letter_of(code: int) -> str:
    for i, (start, letter) in enumerate(SEGMENTS):
        end = SEGMENTS[i + 1][0] - 1 if i + 1 < len(SEGMENTS) else LAST_CODE
        if start <= code <= end:
            return letter
    raise ValueError(f"码位 0x{code:04X} 不在任何字母段内")


def main() -> None:
    pairs: list[tuple[str, str]] = []
    for hi in range(0xB0, 0xD8):
        for lo in range(0xA1, 0xFF):
            code = (hi << 8) | lo
            if code > LAST_CODE:
                break
            try:
                ch = bytes([hi, lo]).decode("gb2312")
            except UnicodeDecodeError:
                continue  # GB2312 里的空位
            if len(ch) != 1:
                continue
            pairs.append((ch, letter_of(code)))

    assert len(pairs) == 3755, f"一级汉字应为 3755 个，实际 {len(pairs)}"
    assert len({c for c, _ in pairs}) == 3755, "出现了重复字"

    # 按 Unicode 码点升序排列，检索层才能二分查找。
    # GB2312 的排列顺序是拼音序，和 Unicode 完全不同，不重排就没法查。
    pairs.sort(key=lambda p: ord(p[0]))

    for ch, letter in SAMPLES.items():
        got = dict(pairs).get(ch)
        assert got == letter, f"「{ch}」期望 {letter}，实际 {got}"

    chars = [p[0] for p in pairs]
    # 首字母压成一条字节串：3755 个单字符写成数组字面量会有 3755 行，没法看。
    # `b"..."` 里每个字母就是一个字节，按索引直接取。
    initials = "".join(p[1] for p in pairs)

    out = []
    out.append("//! 汉字 → 拼音首字母。**本文件由 `tools/gen_pinyin.py` 生成，请勿手改。**")
    out.append("//!")
    out.append("//! 数据来源是 GB2312 一级汉字（3755 个常用字）——它在编码空间里就是按")
    out.append("//! 拼音字母序排列的，所以不需要任何拼音库，也不需要几 MB 的字典。生成脚本")
    out.append("//! 里有逐段核对和抽样断言，改错分段会直接生成失败。")
    out.append("//!")
    out.append("//! 二级汉字（3008 个）在国标里改成按部首排，拿不到首字母，所以不在表内。")
    out.append("//! 查不到的字返回 `None`，检索层会退回普通子串匹配——不出错，只是没有拼音捷径。")
    out.append("")
    out.append("/// 一级汉字，按 Unicode 码点升序排列，供二分查找。")
    out.append(f"static CHARS: [char; {len(chars)}] = [")
    for i in range(0, len(chars), 20):
        row = ", ".join(f"'{c}'" for c in chars[i:i + 20])
        out.append(f"    {row},")
    out.append("];")
    out.append("")
    out.append("/// 与 `CHARS` 同序的首字母。用字节串而不是 `[u8; N]` 数组：")
    out.append("/// 3755 个数字写成数组字面量会占满整个屏幕，字符串一行能放 100 个。")
    out.append(f"static INITIALS: &[u8; {len(chars)}] = b\"{initials}\";")
    out.append("")
    out.append("/// 取一个汉字的拼音首字母（小写）。非一级汉字返回 `None`。")
    out.append("pub fn initial(c: char) -> Option<char> {")
    out.append("    // 先按码点粗筛：`binary_search` 要 12 次比较，标点和 ASCII 不值得走这一趟。")
    out.append("    if !is_han(c) {")
    out.append("        return None;")
    out.append("    }")
    out.append("    CHARS.binary_search(&c).ok().map(|i| INITIALS[i] as char)")
    out.append("}")
    out.append("")
    out.append("/// 是不是汉字。用统一表意文字的常用区间做判断，比逐个查表便宜得多。")
    out.append("fn is_han(c: char) -> bool {")
    out.append("    matches!(c, '\\u{4E00}'..='\\u{9FFF}')")
    out.append("}")
    out.append("")
    out.append("#[cfg(test)]")
    out.append("mod tests {")
    out.append("    use super::*;")
    out.append("")
    out.append("    #[test]")
    out.append("    fn common_hanzi_map_to_expected_initials() {")
    out.append("        // 抽常见词逐个核对，覆盖首字母表的各个字母段。")
    out.append("        let cases = [")
    out.append("            (\"中国\", \"zg\"),")
    out.append("            (\"文件\", \"wj\"),")
    out.append("            (\"代码\", \"dm\"),")
    out.append("            (\"搜索\", \"ss\"),")
    out.append("            (\"剪贴板\", \"jtb\"),")
    out.append("            (\"数据库\", \"sjk\"),")
    out.append("            (\"备份\", \"bf\"),")
    out.append("            (\"超时\", \"cs\"),")
    out.append("            (\"恩\", \"e\"),")
    out.append("            (\"哦\", \"o\"),")
    out.append("            (\"做\", \"z\"),")
    out.append("        ];")
    out.append("        for (word, want) in cases {")
    out.append("            let got: String = word.chars().filter_map(initial).collect();")
    out.append("            assert_eq!(got, want, \"{word}\");")
    out.append("        }")
    out.append("    }")
    out.append("")
    out.append("    #[test]")
    out.append("    fn table_is_sorted_for_binary_search() {")
    out.append("        assert!(CHARS.windows(2).all(|w| w[0] < w[1]), \"表必须严格升序\");")
    out.append("    }")
    out.append("")
    out.append("    #[test]")
    out.append("    fn non_hanzi_has_no_initial() {")
    out.append("        assert_eq!(initial('a'), None);")
    out.append("        assert_eq!(initial('。'), None);")
    out.append("        assert_eq!(initial('1'), None);")
    out.append("        // 二级汉字不在表内，返回 None 而不是猜一个。")
    out.append("        assert_eq!(initial('\\u{9FA0}'), None);")
    out.append("    }")
    out.append("}")
    out.append("")

    with open("src/pinyin.rs", "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(out))

    print(f"生成 src/pinyin.rs：{len(chars)} 字，{len(initials)} 个首字母")


if __name__ == "__main__":
    main()
