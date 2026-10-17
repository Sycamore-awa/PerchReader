//! 导入管线**阶段②③的实际执行者**。
//!
//! [`perch_pipeline`] 只负责"什么时候该做哪件事"，[`perch_import`] 只负责"把文件登记进库"。
//! 这个 crate 负责真正干活：
//!
//! - **阶段②精修**：[`refine_epub`] —— 元数据、封面、目录、字数
//! - **阶段③索引**：[`extract_epub_text`] + [`index_book`] —— 把书脊读成纯文本，
//!   建倒排索引，并能把命中位置还原成 [`Locator`]
//!
//! # 为什么抽正文要单独一层
//!
//! [`perch_index::Index::build`] 接受的是**一整串文本**，而一本书是"多个 XHTML 文件".
//! 直接把各章拼起来会丢掉"这个命中属于哪一章"的信息，搜索结果就跳不过去。
//! 所以这里把拼接结果和**章节区间映射**一起产出（[`BookText`]）：
//!
//! ```text
//! text:     |--第1章--|--第2章--|--第3章--|
//! chapters: [0,120)   [120,300) [300,450)
//! ```
//!
//! 于是 [`locate_hit`] 能用字符偏移反查出章节目录 href。
//!
//! # 边界：抽出的文本是"给索引用的"，不是"给渲染用的"
//!
//! 抽出的纯文本**没有样式、没有图片、没有分页**，只用于建索引和搜索。
//! 阅读器渲染仍然用原始 XHTML。因此这里可以对文本做归一化（合并空白、
//! 用 `\n` 隔开段落），不必保持与渲染结果逐字一致。
//!
//! # 上限与截断
//!
//! 手机上不能因为一本书特别大就把内存吃光。[`ExtractOptions::max_chars`]
//! 限制抽取字符数，超出后停止并在 [`BookText::truncated`] 上报——
//! **明确截断比 OOM 好**，而且索引本来也只是"够用即可"。

#![forbid(unsafe_code)]

pub mod pipeline;

pub use pipeline::{BookStore, Pipeline, StepOutcome, StepSummary};

use perch_epub::{
    EpubArchive, EpubError, Package, TocEntry, XhtmlIndex, flatten_toc, parse_nav, parse_ncx,
};
use perch_index::{Hit, Index};
use perch_model::{Format, Locations, Locator, Perch, TextContext};
use serde::{Deserialize, Serialize};

/// 抽取正文时的选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractOptions {
    /// 最多抽取多少个字符；超出即截断。
    pub max_chars: u64,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        // 500 万字符 ≈ 一本 500 万字的大部头。超过这个量级，
        // 索引本身的收益已经小于内存代价了。
        Self {
            max_chars: 5_000_000,
        }
    }
}

/// 一个章节在 [`BookText::text`] 里占的字符区间。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChapterSpan {
    /// 容器绝对路径，如 `/OEBPS/chap01.xhtml`。
    pub href: String,
    /// 起始字符偏移（含）。
    pub start_char: u64,
    /// 结束字符偏移（不含）。
    pub end_char: u64,
}

impl ChapterSpan {
    /// 这个区间的字符数。
    #[must_use]
    pub const fn char_count(&self) -> u64 {
        self.end_char.saturating_sub(self.start_char)
    }

    /// 字符偏移是否落在本章。
    #[must_use]
    pub const fn contains(&self, char_offset: u64) -> bool {
        char_offset >= self.start_char && char_offset < self.end_char
    }
}

/// 整本书的纯文本 + 章节区间映射。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookText {
    /// 拼接后的正文。各行之间用 `\n` 分隔。
    pub text: String,
    /// 章节区间（按书脊顺序）。
    pub chapters: Vec<ChapterSpan>,
    /// 是否因为超过 [`ExtractOptions::max_chars`] 而被截断。
    pub truncated: bool,
}

impl BookText {
    /// 正文字符数。
    #[must_use]
    pub fn char_count(&self) -> u64 {
        u64::try_from(self.text.chars().count()).unwrap_or(u64::MAX)
    }

    /// 找到包含该字符偏移的章节。
    #[must_use]
    pub fn span_at(&self, char_offset: u64) -> Option<&ChapterSpan> {
        // 章节区间有序且不重叠，二分即可
        let index = self
            .chapters
            .binary_search_by(|span| {
                if char_offset < span.start_char {
                    std::cmp::Ordering::Greater
                } else if char_offset >= span.end_char {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()?;
        self.chapters.get(index)
    }

    /// 该字符偏移在所属章节内的相对偏移。
    #[must_use]
    pub fn offset_in_chapter(&self, char_offset: u64) -> Option<u64> {
        self.span_at(char_offset)
            .map(|span| char_offset.saturating_sub(span.start_char))
    }
}

/// 阶段②的产物：书架和阅读器需要的元数据。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Refinement {
    /// 书名（OPF `dc:title`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 作者（OPF `dc:creator`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// 语言（OPF `dc:language`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// 封面在容器内的绝对路径（来自 `properties="cover-image"` 或 EPUB2 的
    /// `<meta name="cover">`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_href: Option<String>,
    /// 扁平化后的目录标题（层级信息由调用方按需再取）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub toc: Vec<String>,
    /// 书脊项数（含非 XHTML 项）。
    pub spine_count: u32,
    /// 正文字符数（非空白字符）。
    pub char_count: u64,
}

impl Refinement {
    /// 目录条目数。
    #[must_use]
    pub fn toc_count(&self) -> usize {
        self.toc.len()
    }
}

/// 阶段②③的错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobsError {
    /// EPUB 容器或 OPF 解析失败。
    #[error("EPUB 解析失败：{reason}")]
    Epub {
        /// 底层原因。
        reason: String,
    },
    /// 书里没有任何可读的 XHTML 章节。
    #[error("EPUB 里没有可读的章节（书脊为空或全部无法解析）")]
    NoContent,
}

impl JobsError {
    /// 稳定的错误码字符串。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Epub { .. } => "JOBS_EPUB",
            Self::NoContent => "JOBS_NO_CONTENT",
        }
    }

    fn from_epub(error: &EpubError) -> Self {
        Self::Epub {
            reason: error.to_string(),
        }
    }
}

/// 抽取整本书的正文与章节映射（阶段③的输入）。
///
/// 单个章节解析失败会被**跳过**而不是让整本书失败：一本 300 章的书里
/// 有一章 XHTML 畸形，不应该导致整本无法搜索。
///
/// # Errors
///
/// 容器/OPF 解析失败返回 [`JobsError::Epub`]；一章都读不出来返回
/// [`JobsError::NoContent`]。
pub fn extract_epub_text(bytes: &[u8], options: ExtractOptions) -> Result<BookText, JobsError> {
    let mut archive =
        EpubArchive::from_bytes(bytes.to_vec()).map_err(|error| JobsError::from_epub(&error))?;
    let opf_path = archive
        .opf_path()
        .map_err(|error| JobsError::from_epub(&error))?;
    let opf = archive
        .read_to_string(&opf_path)
        .map_err(|error| JobsError::from_epub(&error))?;
    let package = Package::parse(&opf_path, &opf).map_err(|error| JobsError::from_epub(&error))?;

    let mut text = String::new();
    let mut chapters: Vec<ChapterSpan> = Vec::new();
    let mut written: u64 = 0;
    let mut truncated = false;

    for item in package.spine_items() {
        if !item.is_xhtml() {
            continue;
        }
        if truncated {
            break;
        }
        let Ok(content) = archive.read(item.entry_name()) else {
            continue;
        };
        let Ok(index) = XhtmlIndex::build(&content) else {
            continue;
        };
        if index.is_empty() {
            continue;
        }

        let start_char = written;
        for node in index.text_nodes() {
            let node_len = u64::try_from(node.text.chars().count()).unwrap_or(u64::MAX);
            if written.saturating_add(node_len) > options.max_chars {
                truncated = true;
                break;
            }
            text.push_str(&node.text);
            text.push('\n');
            written = written.saturating_add(node_len).saturating_add(1);
        }
        if written > start_char {
            chapters.push(ChapterSpan {
                href: item.href.clone(),
                start_char,
                end_char: written,
            });
        }
    }

    if chapters.is_empty() {
        return Err(JobsError::NoContent);
    }
    Ok(BookText {
        text,
        chapters,
        truncated,
    })
}

/// 阶段②：解析元数据、封面、目录、字数。
///
/// # Errors
///
/// 同 [`extract_epub_text`]；另外如果书名和作者都缺失、且正文也读不出来，
/// 会返回 [`JobsError::NoContent`]——这种情况基本可以判定文件损坏。
pub fn refine_epub(bytes: &[u8], options: ExtractOptions) -> Result<Refinement, JobsError> {
    let mut archive =
        EpubArchive::from_bytes(bytes.to_vec()).map_err(|error| JobsError::from_epub(&error))?;
    let opf_path = archive
        .opf_path()
        .map_err(|error| JobsError::from_epub(&error))?;
    let opf = archive
        .read_to_string(&opf_path)
        .map_err(|error| JobsError::from_epub(&error))?;
    let package = Package::parse(&opf_path, &opf).map_err(|error| JobsError::from_epub(&error))?;

    let title = package.metadata.title.as_deref().and_then(non_empty);
    let author = package
        .metadata
        .creators
        .first()
        .and_then(|value| non_empty(value));
    let language = package.metadata.language.as_deref().and_then(non_empty);

    let cover_href = cover_href(&package);
    let toc = toc_titles(&mut archive, &package);
    let spine_count = u32::try_from(package.spine_items().len()).unwrap_or(u32::MAX);

    // 字数复用抽取逻辑：精修本来就要读一遍正文，没必要再扫第二遍
    let book_text = extract_epub_text(bytes, options).ok();
    let char_count = book_text
        .as_ref()
        .map_or(0, |book| count_visible_chars(&book.text));

    Ok(Refinement {
        title,
        author,
        language,
        cover_href,
        toc,
        spine_count,
        char_count,
    })
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// 非空白字符数。
///
/// 中文书的"字数"按字符算，不按空格切词——按空格切对中文没有意义。
fn count_visible_chars(text: &str) -> u64 {
    u64::try_from(text.chars().filter(|c| !c.is_whitespace()).count()).unwrap_or(u64::MAX)
}

/// 找封面：只认 EPUB3 的 `properties="cover-image"`。
///
/// **不猜**。把"书里第一张图"当封面是常见错误做法，实际会显示成出版社 logo 或
/// 分隔线装饰图。
///
/// # 已知限制
///
/// EPUB2 用 `<meta name="cover" content="item-id">` 声明封面，而 `perch-epub`
/// 的 [`Metadata`](perch_epub::Metadata) 目前不解析这个 meta，因此 EPUB2 的书
/// **取不到封面**。要支持需要在 `perch-epub` 的 OPF 解析里补一个字段——
/// 这比在这里用 id 名字猜要可靠。
fn cover_href(package: &Package) -> Option<String> {
    package
        .manifest
        .iter()
        .find(|item| item.has_property("cover-image"))
        .map(|item| item.href.clone())
}

/// 目录标题（优先 NAV，退回 NCX）。
fn toc_titles<R: std::io::Read + std::io::Seek>(
    archive: &mut EpubArchive<R>,
    package: &Package,
) -> Vec<String> {
    let entries = read_toc(archive, package);
    let flat = flatten_toc(&entries);
    flat.into_iter()
        .filter_map(|entry| non_empty(&entry.title))
        .collect()
}

fn read_toc<R: std::io::Read + std::io::Seek>(
    archive: &mut EpubArchive<R>,
    package: &Package,
) -> Vec<TocEntry> {
    if let Some(nav) = package.nav_item() {
        if let Ok(bytes) = archive.read(nav.entry_name()) {
            let base = parent_dir(&nav.href);
            if let Ok(entries) = parse_nav(&bytes, &base) {
                return entries;
            }
        }
    }
    if let Some(ncx) = package.ncx_item() {
        if let Ok(bytes) = archive.read(ncx.entry_name()) {
            let base = parent_dir(&ncx.href);
            if let Ok(entries) = parse_ncx(&bytes, &base) {
                return entries;
            }
        }
    }
    Vec::new()
}

fn parent_dir(href: &str) -> String {
    match href.rfind('/') {
        Some(index) => href[..=index].to_owned(),
        None => "/".to_owned(),
    }
}

/// 阶段③：为整本书建倒排索引。
///
/// `chunk_chars` 是分片粒度（见 [`perch_index::Index::build`]）。
#[must_use]
pub fn index_book(book: &BookText, chunk_chars: u64) -> Index {
    Index::build(book.text.clone(), chunk_chars)
}

/// 把一次搜索命中还原成 [`Locator`]。
///
/// # 为什么这里不给 `partialCfi`
///
/// 索引里存的是"拼接后正文"的字符偏移，而 CFI 是相对**原始 XHTML 结构**的路径。
/// 从前者推后者需要那一章的 DOM 遍历信息，拼接时已经丢掉（为了省内存）。
///
/// 所以这里给的是**粗定位**：`href`（哪一章）+ `progression`（全书进度）+
/// `text`（命中原文与前后文）。阅读器拿到后用 `perch_epub::reanchor`
/// 在该章内做文本匹配，落到精确位置——这正是设计里的兜底链路
/// （见 `docs/02` §五"失效兜底"）。
#[must_use]
pub fn locate_hit(book: &BookText, hit: &Hit) -> Option<Locator> {
    let span = book.span_at(hit.char_start)?;
    let total = book.char_count();
    let progression = if total == 0 {
        None
    } else {
        Some((hit.char_start as f64 / total as f64).clamp(0.0, 1.0))
    };

    Some(Locator {
        href: span.href.clone(),
        media_type: None,
        title: None,
        locations: Locations {
            progression,
            ..Locations::default()
        },
        text: Some(TextContext {
            before: None,
            highlight: Some(hit.snippet.clone()),
            after: None,
        }),
        perch: Perch {
            format: Some(Format::Epub),
            ..Perch::default()
        },
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    /// 造一个最小 EPUB。
    ///
    /// `chapters` 是每章的正文段落；`with_nav` 控制是否带 EPUB3 目录。
    pub(crate) fn build_epub(chapters: &[&str], with_nav: bool, with_cover: bool) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buffer);
            let stored =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            let deflated = SimpleFileOptions::default();

            // mimetype 必须是第一个条目且不压缩
            zip.start_file("mimetype", stored).unwrap();
            zip.write_all(b"application/epub+zip").unwrap();

            zip.start_file("META-INF/container.xml", deflated).unwrap();
            zip.write_all(
                br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
            )
            .unwrap();

            let mut manifest = String::new();
            let mut spine = String::new();
            for (index, _) in chapters.iter().enumerate() {
                manifest.push_str(&format!(
                    r#"<item id="c{index}" href="chap{index}.xhtml" media-type="application/xhtml+xml"/>"#
                ));
                spine.push_str(&format!(r#"<itemref idref="c{index}"/>"#));
            }
            if with_nav {
                manifest.push_str(r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>"#);
            }
            if with_cover {
                manifest.push_str(r#"<item id="cover" href="cover.png" media-type="image/png" properties="cover-image"/>"#);
            }

            let opf = format!(
                r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">urn:uuid:test</dc:identifier>
    <dc:title>测试书名</dc:title>
    <dc:creator>测试作者</dc:creator>
    <dc:language>zh-CN</dc:language>
  </metadata>
  <manifest>{manifest}</manifest>
  <spine>{spine}</spine>
</package>"#
            );
            zip.start_file("OEBPS/content.opf", deflated).unwrap();
            zip.write_all(opf.as_bytes()).unwrap();

            for (index, body) in chapters.iter().enumerate() {
                let xhtml = format!(
                    r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>第{index}章</title></head>
<body><h1>第{index}章</h1><p>{body}</p></body></html>"#
                );
                zip.start_file(format!("OEBPS/chap{index}.xhtml"), deflated)
                    .unwrap();
                zip.write_all(xhtml.as_bytes()).unwrap();
            }

            if with_nav {
                let nav = r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<body><nav epub:type="toc"><ol>
<li><a href="chap0.xhtml">第一章</a></li>
<li><a href="chap1.xhtml">第二章</a></li>
</ol></nav></body></html>"#;
                zip.start_file("OEBPS/nav.xhtml", deflated).unwrap();
                zip.write_all(nav.as_bytes()).unwrap();
            }
            if with_cover {
                zip.start_file("OEBPS/cover.png", deflated).unwrap();
                zip.write_all(b"\x89PNG\r\n\x1a\n").unwrap();
            }
            zip.finish().unwrap();
        }
        buffer.into_inner()
    }

    // ---------- 正文抽取 ----------

    #[test]
    fn extracts_text_in_spine_order() {
        let epub = build_epub(&["第一章内容", "第二章内容"], false, false);
        let book = extract_epub_text(&epub, ExtractOptions::default()).unwrap();
        assert_eq!(book.chapters.len(), 2);
        assert!(book.text.contains("第一章内容"));
        assert!(book.text.contains("第二章内容"));
        assert!(book.text.find("第一章内容") < book.text.find("第二章内容"));
        assert!(!book.truncated);
    }

    #[test]
    fn chapter_spans_are_ordered_and_locate_offsets() {
        let epub = build_epub(&["甲章节文字", "乙章节文字"], false, false);
        let book = extract_epub_text(&epub, ExtractOptions::default()).unwrap();
        assert_eq!(book.chapters[0].start_char, 0);
        assert!(book.chapters[0].end_char <= book.chapters[1].start_char);
        assert_eq!(book.chapters[0].href, "/OEBPS/chap0.xhtml");
        assert_eq!(book.chapters[1].href, "/OEBPS/chap1.xhtml");

        // 偏移落在第一章
        let span = book.span_at(0).unwrap();
        assert_eq!(span.href, "/OEBPS/chap0.xhtml");
        assert_eq!(book.offset_in_chapter(0), Some(0));
        // 越界返回 None
        assert!(book.span_at(u64::MAX).is_none());
    }

    #[test]
    fn truncation_is_reported_not_silent() {
        let epub = build_epub(&["很长很长的第一章内容", "第二章内容"], false, false);
        let book = extract_epub_text(&epub, ExtractOptions { max_chars: 5 }).unwrap();
        assert!(book.truncated, "超限必须显式上报");
        assert!(book.char_count() <= 10);
    }

    #[test]
    fn broken_epub_reports_error() {
        let result = extract_epub_text(b"not a zip", ExtractOptions::default());
        assert_eq!(result.unwrap_err().code(), "JOBS_EPUB");
    }

    #[test]
    fn empty_spine_is_rejected_by_package_parser() {
        // 书脊为空时 perch-epub 在 Package::parse 阶段就报错，
        // 因此这里拿到的是 JOBS_EPUB 而不是 JOBS_NO_CONTENT。
        let epub = build_epub(&[], false, false);
        let result = extract_epub_text(&epub, ExtractOptions::default());
        assert_eq!(result.unwrap_err().code(), "JOBS_EPUB");
    }

    #[test]
    fn all_chapters_unparsable_reports_no_content() {
        // 书脊非空，但唯一的章节 XHTML 是畸形的 → 被跳过 → 没有任何内容
        let epub = build_epub(&["<<<这不是 XML>>>"], false, false);
        let result = extract_epub_text(&epub, ExtractOptions::default());
        assert_eq!(result.unwrap_err().code(), "JOBS_NO_CONTENT");
    }

    // ---------- 阶段②精修 ----------

    #[test]
    fn refinement_reads_metadata() {
        let epub = build_epub(&["正文"], false, false);
        let refinement = refine_epub(&epub, ExtractOptions::default()).unwrap();
        assert_eq!(refinement.title.as_deref(), Some("测试书名"));
        assert_eq!(refinement.author.as_deref(), Some("测试作者"));
        assert_eq!(refinement.language.as_deref(), Some("zh-CN"));
        assert_eq!(refinement.spine_count, 1);
        assert!(refinement.char_count > 0);
    }

    #[test]
    fn refinement_reads_nav_toc() {
        let epub = build_epub(&["甲", "乙"], true, false);
        let refinement = refine_epub(&epub, ExtractOptions::default()).unwrap();
        assert_eq!(refinement.toc_count(), 2);
        assert_eq!(refinement.toc, vec!["第一章", "第二章"]);
    }

    #[test]
    fn refinement_reads_epub3_cover() {
        let epub = build_epub(&["正文"], false, true);
        let refinement = refine_epub(&epub, ExtractOptions::default()).unwrap();
        assert_eq!(refinement.cover_href.as_deref(), Some("/OEBPS/cover.png"));
    }

    #[test]
    fn refinement_without_cover_does_not_guess() {
        let epub = build_epub(&["正文"], false, false);
        let refinement = refine_epub(&epub, ExtractOptions::default()).unwrap();
        assert_eq!(refinement.cover_href, None, "没声明封面就不要猜");
    }

    #[test]
    fn char_count_ignores_whitespace() {
        // 中文 2 + "abc" 3 + "def" 3 = 8（空格与换行不计）
        assert_eq!(count_visible_chars("中文 abc\ndef "), 8);
        assert_eq!(count_visible_chars("   \n\t "), 0);
    }

    // ---------- 阶段③索引 ----------

    #[test]
    fn index_book_finds_phrases() {
        let epub = build_epub(&["春天的花开", "夏天的雨落"], false, false);
        let book = extract_epub_text(&epub, ExtractOptions::default()).unwrap();
        let index = index_book(&book, 64);

        let hits = index.search("夏天的雨落");
        assert_eq!(hits.len(), 1, "应当能检索到第二章的短语");

        // 命中能还原成该章的 Locator
        let locator = locate_hit(&book, &hits[0]).unwrap();
        assert_eq!(locator.href, "/OEBPS/chap1.xhtml");
        assert_eq!(locator.perch.format, Some(Format::Epub));
        assert!(locator.locations.progression.is_some());
        // 不给 CFI：这是粗定位，精确位置由阅读器按文本重锚定
        assert_eq!(locator.locations.partial_cfi, None);
        assert!(locator.text.and_then(|t| t.highlight).is_some());
    }

    #[test]
    fn cross_chapter_search_maps_to_the_right_chapter() {
        let epub = build_epub(&["第一章讲苹果", "第二章讲香蕉"], false, false);
        let book = extract_epub_text(&epub, ExtractOptions::default()).unwrap();
        let index = index_book(&book, 64);

        let apple = index.search("苹果");
        assert_eq!(apple.len(), 1);
        assert_eq!(
            locate_hit(&book, &apple[0]).unwrap().href,
            "/OEBPS/chap0.xhtml"
        );

        let banana = index.search("香蕉");
        assert_eq!(banana.len(), 1);
        assert_eq!(
            locate_hit(&book, &banana[0]).unwrap().href,
            "/OEBPS/chap1.xhtml"
        );
    }

    #[test]
    fn search_miss_returns_nothing() {
        let epub = build_epub(&["正文内容"], false, false);
        let book = extract_epub_text(&epub, ExtractOptions::default()).unwrap();
        let index = index_book(&book, 64);
        assert!(index.search("完全不存在的词").is_empty());
    }
}
