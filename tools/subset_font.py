"""把系统中文字体裁剪成项目内置的子集字体。

只保留 GB2312 全集（6763 汉字 + 符号）+ ASCII + 常用标点，共 7,707 字。
这覆盖日常剪贴板内容的 99.9%，体积从 16.9MB 降到 2.4MB 左右。

当前字体：Noto Sans SC（思源黑体，SIL Open Font License 1.1，
见 assets/FONT-LICENSE.txt）。选它是因为 OFL 允许修改、嵌入 exe 并随软件
再分发——微软雅黑等 Windows 自带字体只能本机使用，不能分发。

系统自带的是**可变字体**（只有 wght 轴 100–900），egui 不认可变轴，
所以必须先实例化到 400 再做子集化；顺序反了会留下坏的 fvar / HVAR 表。
实例化后是标准 TrueType glyf 轮廓，egui 能直接加载。

用法：
    python tools/subset_font.py

产物：assets/font.ttf
"""

from pathlib import Path

from fontTools import subset
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont

SRC = Path(r"C:\Windows\Fonts\NotoSansSC-VF.ttf")
WEIGHT = 400
OUT = Path(__file__).resolve().parent.parent / "assets" / "font.ttf"


def build_charset() -> str:
    chars: set[str] = set()

    # ASCII 可见字符
    chars.update(chr(c) for c in range(0x20, 0x7F))

    # GB2312 全部：一级汉字 3755 + 二级汉字 3008 + 全角符号 + 假名希腊字母等
    for hi in range(0xA1, 0xF8):
        for lo in range(0xA1, 0xFF):
            try:
                chars.add(bytes([hi, lo]).decode("gb2312"))
            except UnicodeDecodeError:
                continue

    # GB2312 没覆盖、但剪贴板里常见的符号
    chars.update(
        "—…·、。，；：？！“”‘’（）《》〈〉【】〔〕％＋－×÷＝≠≤≥∞°′″℃№℡"
        "←↑→↓↔⇔■□◆◇●○★☆※§¶©®™€£¥¢"
        "①②③④⑤⑥⑦⑧⑨⑩⑴⑵⑶⑷⑸ⅠⅡⅢⅣⅤ"
    )

    # 通用标点 / 箭头 / 数学运算符区段
    chars.update(chr(c) for c in range(0x2000, 0x2070))
    chars.update(chr(c) for c in range(0x2190, 0x21A0))
    chars.update(chr(c) for c in range(0x2200, 0x2270))

    # 去掉控制字符
    chars = {c for c in chars if c.isprintable() or c == " "}
    return "".join(sorted(chars))


def main() -> None:
    if not SRC.exists():
        raise SystemExit(f"找不到源字体 {SRC}")

    text = build_charset()
    print(f"字符集大小: {len(text)}")

    font = TTFont(str(SRC))
    if "fvar" in font:
        print(f"检测到可变字体，实例化到 wght={WEIGHT}")
        font = instantiateVariableFont(font, {"wght": WEIGHT}, inplace=True)

    opts = subset.Options()
    opts.layout_features = ["*"]
    opts.hinting = False          # 去掉 hinting，屏幕渲染由 egui 自己做
    opts.desubroutinize = True
    opts.drop_tables += ["DSIG"]

    subsetter = subset.Subsetter(options=opts)
    subsetter.populate(text=text)
    subsetter.subset(font)

    OUT.parent.mkdir(parents=True, exist_ok=True)
    font.save(str(OUT))
    font.close()

    src_kb = SRC.stat().st_size / 1024
    out_kb = OUT.stat().st_size / 1024
    print(
        f"源字体 {src_kb:.0f} KB → 子集 {out_kb:.0f} KB"
        f"（省 {100 - out_kb / src_kb * 100:.0f}%，wght={WEIGHT}）"
    )
    print(f"输出: {OUT}")


if __name__ == "__main__":
    main()
