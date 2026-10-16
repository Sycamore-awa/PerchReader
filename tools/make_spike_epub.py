#!/usr/bin/env python3
"""生成用于 spike / 测试的中文 EPUB 3。

为什么要自己生成而不是找一本现成的电子书：
1. **版权**——现成的电子书不能进仓库；
2. **可控**——章节数、文字长度、编码、目录结构都由我们决定，
   便于精确验证分页、CFI、标注锚点等行为；
3. **可复现**——脚本在仓库里，任何时候都能重新生成同一个文件。

正文文字为手写占位内容，无版权问题。

用法：python3 tools/make_spike_epub.py <输出路径>
"""

from __future__ import annotations

import sys
import zipfile
from pathlib import Path

CONTAINER = """<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"""

# 手写正文：分段、含标点与中英混排，用来观察中文排版与断行
CHAPTERS: list[tuple[str, str, list[str]]] = [
    (
        "chap01",
        "第一章 潮汐与书页",
        [
            "灯塔的守夜人把最后一本书摊开在膝上。海风从窄窗挤进来，纸页便一页一页地翻过去，像是有人比他更急着读完。",
            "他从不催促。书这东西，急不得。潮水涨落各有其时，字句也需要时间沉下去。",
            "桌上放着一枚黄铜书签，边缘已被摩挲得发亮。三十七年来，他每晚只读两页，然后合上书，把书签夹在中间。",
            "「Two pages a day」——他在账本上这样写，用英文，因为中文里没有恰好对应的说法，"
            "「每天两页」听起来太像一句训诫。",
        ],
    ),
    (
        "chap02",
        "第二章 索引的隐喻",
        [
            "镇上的邮差负责送信，也负责记住每一封信的来处。他说，人这一生其实就是在给自己编一份索引。",
            "关键词越多，检索越慢；可关键词太少，又被淹没在无关的段落里。",
            "所以他给每个人都留了三行：一行写名字，一行写他们最常提到的词，最后一行空着——给还没发生的事。",
        ],
    ),
    (
        "chap03",
        "第三章 同步",
        [
            "两台机器，同一本书，读到不同的位置。这大概是所有读者都会遇到的、最小规模的分歧。",
            "有人主张以最后读到的位置为准，因为人是连续的；有人主张以最远的位置为准，因为记忆不该倒退。",
            "守夜人的做法更笨：他把两处都记下来，并在书签上写一个小小的问号，等下一次翻开时再决定。",
            "「冲突不是错误，」他说，「冲突只是还没有被读到的部分。」",
        ],
    ),
]


def chapter_xhtml(title: str, paragraphs: list[str]) -> str:
    body = "\n".join(f"    <p>{text}</p>" for text in paragraphs)
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="zh-Hans">
  <head>
    <title>{title}</title>
    <link rel="stylesheet" type="text/css" href="../styles/main.css"/>
  </head>
  <body>
    <h1>{title}</h1>
{body}
  </body>
</html>
"""


def nav_xhtml() -> str:
    items = "\n".join(
        f'        <li><a href="text/{cid}.xhtml">{title}</a></li>'
        for cid, title, _ in CHAPTERS
    )
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="zh-Hans">
  <head><title>目录</title></head>
  <body>
    <nav epub:type="toc" id="toc">
      <h1>目录</h1>
      <ol>
{items}
      </ol>
    </nav>
  </body>
</html>
"""


def opf() -> str:
    manifest_items = "\n".join(
        f'    <item id="{cid}" href="text/{cid}.xhtml" media-type="application/xhtml+xml"/>'
        for cid, _, _ in CHAPTERS
    )
    spine_items = "\n".join(f'    <itemref idref="{cid}"/>' for cid, _, _ in CHAPTERS)
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid" xml:lang="zh-Hans">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:9f1c5b7e-0000-4000-8000-perchspike01</dc:identifier>
    <dc:title>灯塔守夜人的两页书</dc:title>
    <dc:creator>PerchReader 测试用例</dc:creator>
    <dc:language>zh-Hans</dc:language>
    <dc:description>由 tools/make_spike_epub.py 生成的中文测试书，用于验证渲染、分页、CFI 与标注。</dc:description>
    <meta property="dcterms:modified">2026-10-16T00:00:00Z</meta>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="css" href="styles/main.css" media-type="text/css"/>
{manifest_items}
  </manifest>
  <spine>
{spine_items}
  </spine>
</package>
"""


CSS = """/* 中文排版基础设置：行高放宽，首行缩进两字 */
html { font-size: 100%; }
body {
  line-height: 1.8;
  margin: 0 1em;
  font-family: "Noto Serif CJK SC", "Source Han Serif SC", serif;
}
h1 { font-size: 1.5em; margin: 1.2em 0 1em; line-height: 1.4; }
p { text-indent: 2em; margin: 0.6em 0; }
"""


def build(destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)

    with zipfile.ZipFile(destination, "w", zipfile.ZIP_DEFLATED) as archive:
        # 规范要求：mimetype 必须是第一个条目，且不得压缩（便于被嗅探识别）
        archive.writestr(
            zipfile.ZipInfo("mimetype", date_time=(2026, 10, 16, 0, 0, 0)),
            "application/epub+zip",
            compress_type=zipfile.ZIP_STORED,
        )
        archive.writestr("META-INF/container.xml", CONTAINER)
        archive.writestr("OEBPS/content.opf", opf())
        archive.writestr("OEBPS/nav.xhtml", nav_xhtml())
        archive.writestr("OEBPS/styles/main.css", CSS)
        for cid, title, paragraphs in CHAPTERS:
            archive.writestr(
                f"OEBPS/text/{cid}.xhtml", chapter_xhtml(title, paragraphs)
            )

    size = destination.stat().st_size
    print(f"已生成 {destination}（{size} 字节，{len(CHAPTERS)} 章）")


if __name__ == "__main__":
    target = Path(sys.argv[1] if len(sys.argv) > 1 else "spike_book.epub")
    build(target)
