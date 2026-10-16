//! 测试用的**合成 EPUB**。
//!
//! 为什么不提交真实 EPUB 作为测试资源：
//! 1. 版权——真实电子书不能进仓库；
//! 2. 可控——合成书能精确覆盖边界（`./`、`../`、`properties`、`linear="no"`、
//!    缺 container.xml、非 UTF-8 字节等），真实书很难凑齐；
//! 3. 快——内存里拼一个 ZIP 只要几毫秒。
//!
//! 这里的书刻意做成 **EPUB 3 + 兼容 EPUB 2 的 NCX**，以便后续同时覆盖 NAV 与 NCX 两条目录路径。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Cursor, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// 容器描述文件。
pub(crate) const CONTAINER_XML: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"##;

/// OPF 包文档。故意包含若干"难缠"的写法：
/// - `dc:` 前缀的元数据
/// - `./text/chap02.xhtml`（多余的 `./`）
/// - `../styles/main.css`（越过 OPF 所在目录）
/// - `properties="scripted mathml"`（多值属性）
/// - `linear="no"`（非线性章节）
pub(crate) const OPF: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>测试书籍</dc:title>
    <dc:creator>张三</dc:creator>
    <dc:creator>李四</dc:creator>
    <dc:language>zh-Hans</dc:language>
    <dc:identifier id="bookid">urn:uuid:12345678-1234-1234-1234-123456789abc</dc:identifier>
    <dc:publisher>测试出版社</dc:publisher>
    <dc:description>一本书架上的假书。</dc:description>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="chap1" href="text/chap01.xhtml" media-type="application/xhtml+xml"/>
    <item id="chap2" href="./text/chap02.xhtml" media-type="application/xhtml+xml"/>
    <item id="chap3" href="text/chap03.xhtml" media-type="application/xhtml+xml" properties="scripted mathml"/>
    <item id="css" href="../styles/main.css" media-type="text/css"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="chap1"/>
    <itemref idref="chap2"/>
    <itemref idref="chap3" linear="no"/>
  </spine>
</package>
"##;

/// EPUB3 的导航文档（NAV）。目前只作为夹具存在，TOC 解析尚未实现。
pub(crate) const NAV: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>目录</title></head>
<body>
  <nav epub:type="toc" id="toc">
    <h1>目录</h1>
    <ol>
      <li><a href="text/chap01.xhtml">第一章</a></li>
      <li><a href="text/chap02.xhtml">第二章</a>
        <ol>
          <li><a href="text/chap02.xhtml#s1">第一节</a></li>
        </ol>
      </li>
    </ol>
  </nav>
</body>
</html>
"##;

/// EPUB2 的 NCX。
pub(crate) const NCX: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head><meta name="dtb:uid" content="urn:uuid:12345678-1234-1234-1234-123456789abc"/></head>
  <docTitle><text>测试书籍</text></docTitle>
  <navMap>
    <navPoint id="np1" playOrder="1">
      <navLabel><text>第一章</text></navLabel>
      <content src="text/chap01.xhtml"/>
    </navPoint>
    <navPoint id="np2" playOrder="2">
      <navLabel><text>第二章</text></navLabel>
      <content src="text/chap02.xhtml"/>
      <navPoint id="np2-1" playOrder="3">
        <navLabel><text>第一节</text></navLabel>
        <content src="text/chap02.xhtml#s1"/>
      </navPoint>
    </navPoint>
  </navMap>
</ncx>
"##;

const CHAPTER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>章节</title></head>
<body><h1>标题</h1><p>正文内容。</p></body></html>
"#;

/// 把一个结构完整的最小 EPUB 组装成字节。
pub(crate) fn minimal_epub() -> Vec<u8> {
    build(&[
        ("META-INF/container.xml", CONTAINER_XML.as_bytes()),
        ("OEBPS/content.opf", OPF.as_bytes()),
        ("OEBPS/nav.xhtml", NAV.as_bytes()),
        ("OEBPS/toc.ncx", NCX.as_bytes()),
        ("OEBPS/text/chap01.xhtml", CHAPTER.as_bytes()),
        ("OEBPS/text/chap02.xhtml", CHAPTER.as_bytes()),
        ("OEBPS/text/chap03.xhtml", CHAPTER.as_bytes()),
    ])
}

/// 缺少 `META-INF/container.xml` 的"伪 EPUB"，用于错误路径测试。
pub(crate) fn epub_without_container() -> Vec<u8> {
    build(&[("OEBPS/content.opf", OPF.as_bytes())])
}

/// 按 ZIP 结构组装条目。
///
/// 注意：`mimetype` 必须是**第一个**条目并以 **Stored（不压缩）** 方式存储，
/// 这是 EPUB 规范对"可以被嗅探识别"的要求。
pub(crate) fn build(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    writer.start_file("mimetype", stored).unwrap();
    writer.write_all(b"application/epub+zip").unwrap();

    for (name, content) in entries {
        writer.start_file(*name, deflated).unwrap();
        writer.write_all(content).unwrap();
    }

    writer.finish().unwrap().into_inner()
}
