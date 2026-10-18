//! 导入管线**阶段②③的实际执行者**。
//!
//! [`perch_pipeline`] 只负责"什么时候该做哪件事"，[`perch_import`] 只负责"把文件登记进库"。
//! 这个 crate 负责真正干活：
//!
//! - **阶段②精修**：[`refine_epub`] / [`refine_txt`] —— 元数据、封面、目录、字数、编码
//! - **阶段③索引**：[`extract_epub_text`] / [`extract_txt`] + [`index_book`] ——
//!   把书读成纯文本，建倒排索引，并能把命中位置还原成 [`Locator`]
//!
//! # 为什么抽正文要单独一层
//!
//! [`perch_index::Index::build`] 接受的是**一整串文本**，而一本 EPUB 是"多个 XHTML 文件"。
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
//! # TXT 的"章节"
//!
//! TXT 没有内嵌的篇章结构，所以整本书就是**一个章节**：[`ChapterSpan::href`] 固定为
//! `txt:`，区间为 `[0, 字符数)`。这也与 [`Locator`] 对 TXT 的约定一致——整个文件是
//! 一个资源，位置放在 `perch.txtOffset` 里。
//!
//! # 边界：抽出的文本是"给索引用的"，不是"给渲染用的"
//!
//! 抽出的纯文本**没有样式、没有图片、没有分页**，只用于建索引和搜索。
//! 阅读器渲染仍然用原始 XHTML / TXT。因此这里可以对文本做归一化（合并空白、
//! 用 `\n` 隔开段落），不必保持与渲染结果逐字一致。
//!
//! # 上限与截断
//!
//! 手机上不能因为一本书特别大就把内存吃光。[`ExtractOptions::max_chars`]
//! 限制抽取字符数，超出后停止并在 [`BookText::truncated`] 上报——
//! **明确截断比 OOM 好**，而且索引本来也只是"够用即可"。

#![forbid(unsafe_code)]

pub mod pipeline;
pub mod store;

pub use pipeline::{BookStore, Pipeline, StepOutcome, StepSummary};
pub use store::{PathStore, Recovered, StateFile};

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookText {
    /// 书籍格式。`locate_hit` 依据它决定命中位置怎么还原成 [`Locator`]。
    ///
    /// 反序列化时缺省为 EPUB：本字段是后加的，旧版本落盘的 [`BookText`]
    /// （当时只支持 EPUB）没有这一项，补齐成 EPUB 才能继续读出来。
    #[serde(default = "default_book_format")]
    pub format: Format,
    /// 拼接后的正文。各行之间用 `\n` 分隔。
    pub text: String,
    /// 章节区间（按书脊顺序；TXT 只有一个 `txt:` 区间）。
    pub chapters: Vec<ChapterSpan>,
    /// 是否因为超过 [`ExtractOptions::max_chars`] 而被截断。
    pub truncated: bool,
}

/// 旧 [`BookText`]（只有 EPUB）没有 `format` 字段时的缺省值。
fn default_book_format() -> Format {
    Format::Epub
}

impl Default for BookText {
    fn default() -> Self {
        Self {
            format: default_book_format(),
            text: String::new(),
            chapters: Vec::new(),
            truncated: false,
        }
    }
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
    /// 探测到的源文件编码名（`encoding_rs` 的名字，如 `UTF-8` / `GBK` / `Big5`）。
    ///
    /// 只有 TXT 有这一项；EPUB 内嵌 XHTML 由规范要求 UTF-8，因此为 `None`。
    /// 值是**探测结果**而非保证——需要判断是否可信时，调用方可再走
    /// `perch_txt::decode(...).certain()`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
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
    /// 文件里没有任何非空白的正文（空文件、纯空白文件，或截断后为空）。
    #[error("文件里没有可读的正文内容（空文件或只有空白）")]
    EmptyContent,
    /// 该格式不支持精修/索引。
    ///
    /// PDF 目前只做**结构**解析（页数/大纲/元数据，见 `perch-pdf`），
    /// 不做正文抽取，因此不能进阶段②③。
    #[error("暂不支持对该格式做精修/索引：{format}")]
    UnsupportedFormat {
        /// 格式名。
        format: &'static str,
    },
}

impl JobsError {
    /// 稳定的错误码字符串。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Epub { .. } => "JOBS_EPUB",
            Self::NoContent => "JOBS_NO_CONTENT",
            Self::EmptyContent => "JOBS_EMPTY_CONTENT",
            Self::UnsupportedFormat { .. } => "JOBS_UNSUPPORTED_FORMAT",
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
        format: Format::Epub,
        text,
        chapters,
        truncated,
    })
}

/// 由内容**嗅探**格式。
///
/// 只认两种"文件头"：
/// - ZIP 的 `PK\x03\x04` → EPUB（EPUB 就是 ZIP）
/// - `%PDF-` → PDF
/// - 其余 → TXT
///
/// ⚠️ 这是**兜底**，不是首选：导入阶段已经能从扩展名得到格式，
/// 调用方应当优先用那个。只有在"只有字节、没有文件名"时才用它。
#[must_use]
pub fn sniff_format(bytes: &[u8]) -> Format {
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        return Format::Epub;
    }
    let head = &bytes[..bytes.len().min(1024)];
    if head.windows(5).any(|window| window == b"%PDF-") {
        return Format::Pdf;
    }
    Format::Txt
}

/// 按格式精修（阶段②的统一入口）。
///
/// # Errors
///
/// - [`JobsError::UnsupportedFormat`]：格式不支持（目前是 PDF）
/// - 其余同 [`refine_epub`] / [`refine_txt`]
pub fn refine(
    bytes: &[u8],
    format: Format,
    options: ExtractOptions,
) -> Result<Refinement, JobsError> {
    match format {
        Format::Epub => refine_epub(bytes, options),
        Format::Txt => refine_txt(bytes, options),
        Format::Pdf => Err(JobsError::UnsupportedFormat {
            format: format.as_str(),
        }),
    }
}

/// 按格式抽取正文（阶段③的统一入口）。
///
/// # Errors
///
/// - [`JobsError::UnsupportedFormat`]：格式不支持（目前是 PDF）
/// - 其余同 [`extract_epub_text`] / [`extract_txt`]
pub fn extract_text(
    bytes: &[u8],
    format: Format,
    options: ExtractOptions,
) -> Result<BookText, JobsError> {
    match format {
        Format::Epub => extract_epub_text(bytes, options),
        Format::Txt => extract_txt(bytes, options),
        Format::Pdf => Err(JobsError::UnsupportedFormat {
            format: format.as_str(),
        }),
    }
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

    // 封面判定统一交给 perch-epub：它同时支持 EPUB3 的 properties="cover-image"
    // 与 EPUB2 的 <meta name="cover">，并且会拒绝"指向封面页而非图片"的情况。
    let cover_href = package.cover_image().map(str::to_owned);
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
        // EPUB 的 XHTML 按规范是 UTF-8，没有"源文件编码"需要暴露出选择
        encoding: None,
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

/// 归一化 TXT 的空白。
///
/// 规则：
/// 1. `\r\n`、单独的 `\r` 一律当作 `\n`（TXT 来源五花八门，Windows / 老 Mac
///    换行都可能出现）；
/// 2. 连续的空行压成**至多一个**空行（只由空格 / 制表符组成的"视觉空行"也算空行）；
/// 3. 去掉正文首尾的空行；
/// 4. **行内的空白原样保留**。
///
/// 为什么只压空行、不压行内空白：索引是字面量短语匹配（见 `perch-index`），
/// 英文单词之间的空格、中文排版缩进如果被吞掉，会改变检索语义；而大段空行
/// 只会把分片撑大、让 snippet 变成空白，压成一个空行反而更利于阅读与检索。
fn normalize_txt_whitespace(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut wrote_line = false;
    let mut blank_pending = false;
    for_each_txt_line(raw, |line| {
        if line.trim().is_empty() {
            // 只记录"这里有过空行"，真正输出要等到下一行非空——
            // 这样文末的空行会被自然丢弃，不必再 trim。
            if wrote_line {
                blank_pending = true;
            }
        } else {
            if wrote_line {
                out.push('\n');
                if blank_pending {
                    out.push('\n');
                }
            }
            out.push_str(line);
            wrote_line = true;
            blank_pending = false;
        }
    });
    out
}

/// 按 `\n` / `\r\n` / `\r` 切行，回调收到**不含行尾换行符**的切片。
///
/// 逐字节扫描是安全的：UTF-8 多字节字符的续字节都 `>= 0x80`，不会等于
/// ASCII 的 `\r` (0x0D) / `\n` (0x0A)，所以命中的位置一定是字符边界。
fn for_each_txt_line(raw: &str, mut visit: impl FnMut(&str)) {
    let bytes = raw.as_bytes();
    let mut start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                visit(&raw[start..index]);
                index += 1;
                start = index;
            }
            b'\r' => {
                visit(&raw[start..index]);
                index += 1;
                if bytes.get(index) == Some(&b'\n') {
                    index += 1;
                }
                start = index;
            }
            _ => index += 1,
        }
    }
    // 最后一行（即使为空）也回调：空行会被上面的逻辑压掉，不会多输出换行
    visit(&raw[start..]);
}

/// 截断到前 `max_chars` 个字符（按 `char` 边界，不会切坏 UTF-8）。
fn truncate_chars(text: &str, max_chars: u64) -> String {
    // 32 位平台上 u64 可能超出 usize；超出即视为"不截断"
    match usize::try_from(max_chars) {
        Ok(limit) => text.chars().take(limit).collect(),
        Err(_) => text.to_owned(),
    }
}

/// 把已解码的 TXT 正文归一化、截断，并包装成单章节的 [`BookText`]。
fn build_txt_book_text(decoded: &str, options: ExtractOptions) -> Result<BookText, JobsError> {
    let normalized = normalize_txt_whitespace(decoded);
    let total = u64::try_from(normalized.chars().count()).unwrap_or(u64::MAX);
    let truncated = total > options.max_chars;
    let text = if truncated {
        truncate_chars(&normalized, options.max_chars)
    } else {
        normalized
    };
    if text.trim().is_empty() {
        return Err(JobsError::EmptyContent);
    }
    let char_count = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
    Ok(BookText {
        format: Format::Txt,
        // TXT 没有篇章结构：整本书是一个资源、一个章节
        chapters: vec![ChapterSpan {
            href: "txt:".to_owned(),
            start_char: 0,
            end_char: char_count,
        }],
        text,
        truncated,
    })
}

/// 抽取整个 TXT 文件的正文与"章节"映射（阶段③的输入）。
///
/// 编码由 [`perch_txt::decode`] 探测并解码（BOM → 合法 UTF-8 → `chardetng` 猜测）；
/// 解码**不会失败**，非法序列会被替换为 U+FFFD。这里只需要解码后的文本，
/// 因此刻意**不**走 [`perch_txt::TxtDocument`]——那会顺带建立字符索引与行索引，
/// 对导入阶段是白花的 CPU 和内存。探测是否可信可看 `perch_txt::decode(...).certain()`。
///
/// TXT 没有篇章结构，因此整本书是**一个章节**：`href` 固定为 `txt:`，
/// 区间为 `[0, 字符数)`。
///
/// # 空白归一化
///
/// 见 [`normalize_txt_whitespace`]：统一换行为 `\n`，连续空行压成一个空行，
/// 去掉首尾空行，行内空白原样保留。
///
/// # 截断
///
/// 超过 [`ExtractOptions::max_chars`] 时只保留前 `max_chars` 个字符，并在
/// [`BookText::truncated`] 上报。截断按 `char` 边界进行，不会产生非法 UTF-8。
///
/// # Errors
///
/// 解码 + 归一化后没有任何非空白字符（空文件、纯空白文件，或 `max_chars` 为 0
/// 导致截断为空）时返回 [`JobsError::EmptyContent`]。
pub fn extract_txt(bytes: &[u8], options: ExtractOptions) -> Result<BookText, JobsError> {
    let decoded = perch_txt::decode(bytes);
    build_txt_book_text(decoded.text(), options)
}

/// 阶段②：TXT 的"精修"。
///
/// TXT 没有内嵌元数据，所以 `title` / `author` / `language` / `cover_href` 都留空，
/// 标题由调用方从文件名补；`toc` 为空；`spine_count` 固定为 1。
///
/// `char_count` 用归一化后正文的**非空白字符数**，与 EPUB 口径一致。
/// `encoding` 给出 `perch-txt` 探测到的源编码名，供 UI 在乱码时提供"换个编码重读"。
/// 与 [`extract_txt`] 一样只走 [`perch_txt::decode`]，不建无用的索引。
///
/// # Errors
///
/// 同 [`extract_txt`]：没有可读正文时返回 [`JobsError::EmptyContent`]。
pub fn refine_txt(bytes: &[u8], options: ExtractOptions) -> Result<Refinement, JobsError> {
    let decoded = perch_txt::decode(bytes);
    let encoding = Some(decoded.encoding().to_owned());
    let book = build_txt_book_text(decoded.text(), options)?;
    Ok(Refinement {
        title: None,
        author: None,
        language: None,
        encoding,
        cover_href: None,
        toc: Vec::new(),
        spine_count: 1,
        char_count: count_visible_chars(&book.text),
    })
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
/// 按 [`BookText::format`] 分派：
///
/// - **EPUB**：给粗定位 `href`（哪一章）+ `progression`（全书进度）+
///   `text.highlight`（命中原文）。不给 `partialCfi`，原因见下。
/// - **TXT**：用 [`Locator::new_txt`] 构造，位置放在 `perch.txtOffset`
///   （以及便于快速切片的 `perch.txtByteOffset`），`href` 固定为 `txt:`。
/// - **PDF**：不走这条链路（PDF 定位由 `perch-pdf` 负责），返回 `None`。
///
/// # 为什么 EPUB 不给 `partialCfi`
///
/// 索引里存的是"拼接后正文"的字符偏移，而 CFI 是相对**原始 XHTML 结构**的路径。
/// 从前者推后者需要那一章的 DOM 遍历信息，拼接时已经丢掉（为了省内存）。
///
/// 所以这里给的是**粗定位**：`href`（哪一章）+ `progression`（全书进度）+
/// `text`（命中原文与前后文）。阅读器拿到后用 `perch_epub::reanchor`
/// 在该章内做文本匹配，落到精确位置——这正是设计里的兜底链路
/// （见 `docs/02` §五"失效兜底"）。
///
/// # TXT 的字节偏移
///
/// `perch.txtByteOffset` 是相对**归一化后的 [`BookText::text`]** 的字节偏移，
/// 不是原始文件字节偏移（解码 + 空白归一化会改变长度）。若调用方需要拿原始
/// 文件做切片，应以 `txtOffset`（字符偏移）为准，再用 `perch_txt` 重新换算。
#[must_use]
pub fn locate_hit(book: &BookText, hit: &Hit) -> Option<Locator> {
    match book.format {
        Format::Epub => locate_epub_hit(book, hit),
        Format::Txt => locate_txt_hit(book, hit),
        Format::Pdf => None,
    }
}

/// EPUB 命中 → 粗定位 [`Locator`]（刻意不给 CFI）。
fn locate_epub_hit(book: &BookText, hit: &Hit) -> Option<Locator> {
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

/// TXT 命中 → [`Locator::new_txt`]。
fn locate_txt_hit(book: &BookText, hit: &Hit) -> Option<Locator> {
    let total = book.char_count();
    if total == 0 || hit.char_start >= total {
        return None;
    }
    // 字节偏移相对 BookText::text（已归一化）。命中偏移一定落在字符边界上，
    // 因为索引的 token 都来自 text 的 char_indices。
    let byte_offset = usize::try_from(hit.char_start)
        .ok()
        .and_then(|char_index| book.text.char_indices().nth(char_index))
        .and_then(|(byte, _)| u64::try_from(byte).ok());
    let mut locator = Locator::new_txt(hit.char_start, byte_offset, Some(total));
    // new_txt 已填好 href（txt:）/ perch.format / progression；
    // 这里补上与 EPUB 一致的容错重定位原文片段。
    locator.text = Some(TextContext {
        before: None,
        highlight: Some(hit.snippet.clone()),
        after: None,
    });
    Some(locator)
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

    // ---------- TXT：正文抽取 ----------

    #[test]
    fn extract_txt_makes_single_chapter() {
        let book = extract_txt(
            "第一段\r\n\r\n\r\n第二段".as_bytes(),
            ExtractOptions::default(),
        )
        .unwrap();

        assert_eq!(book.format, Format::Txt);
        assert_eq!(book.chapters.len(), 1, "TXT 整本只有一个章节");
        let span = &book.chapters[0];
        assert_eq!(span.href, "txt:");
        assert_eq!(span.start_char, 0);
        assert_eq!(span.end_char, book.char_count());
        assert!(!book.truncated);
        assert_eq!(
            book.text, "第一段\n\n第二段",
            "CRLF 统一为 \\n，连续空行压成一个"
        );
    }

    #[test]
    fn extract_txt_truncates_on_char_boundary() {
        let bytes = "中文abc中文".as_bytes();
        let book = extract_txt(bytes, ExtractOptions { max_chars: 4 }).unwrap();
        assert!(book.truncated, "超过 max_chars 必须显式上报");
        assert_eq!(book.text, "中文ab");
        assert_eq!(book.char_count(), 4);
        assert_eq!(book.chapters[0].end_char, 4, "截断后章节区间要跟着缩短");

        // 未超限时不应上报截断
        let full = extract_txt(bytes, ExtractOptions { max_chars: 7 }).unwrap();
        assert!(!full.truncated);
        assert_eq!(full.text, "中文abc中文");
    }

    #[test]
    fn extract_txt_strips_utf8_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("带 BOM 的文本".as_bytes());

        let book = extract_txt(&bytes, ExtractOptions::default()).unwrap();
        assert_eq!(book.text, "带 BOM 的文本", "BOM 不应进入正文");
        assert!(!book.text.contains('\u{feff}'));
    }

    #[test]
    fn normalize_collapses_blank_lines_and_keeps_inline_spaces() {
        let raw = "\r\n\r\n  第一段  有空格 \r\n \r\n\t\r\n第二段\r\n\r\n\r\n";
        let normalized = normalize_txt_whitespace(raw);
        assert_eq!(
            normalized, "  第一段  有空格 \n\n第二段",
            "首尾空行丢弃，视觉空行合并成一个空行，行内空格保留"
        );
    }

    #[test]
    fn extract_txt_empty_content_is_error() {
        assert_eq!(
            extract_txt(b"", ExtractOptions::default())
                .unwrap_err()
                .code(),
            "JOBS_EMPTY_CONTENT"
        );
        assert_eq!(
            extract_txt(b" \r\n\t\n ", ExtractOptions::default())
                .unwrap_err()
                .code(),
            "JOBS_EMPTY_CONTENT"
        );
        // max_chars = 0 截断后为空，同样视为没有内容
        assert_eq!(
            extract_txt("有内容".as_bytes(), ExtractOptions { max_chars: 0 })
                .unwrap_err()
                .code(),
            "JOBS_EMPTY_CONTENT"
        );
        // 精修复用同一条错误路径
        assert_eq!(
            refine_txt(b"", ExtractOptions::default())
                .unwrap_err()
                .code(),
            "JOBS_EMPTY_CONTENT"
        );
    }

    // ---------- TXT：阶段②精修 ----------

    #[test]
    fn refine_txt_char_count_ignores_whitespace() {
        let refinement =
            refine_txt("中文 abc\r\ndef ".as_bytes(), ExtractOptions::default()).unwrap();
        assert_eq!(
            refinement.title, None,
            "TXT 没有内嵌标题，由调用方从文件名取"
        );
        assert_eq!(refinement.spine_count, 1);
        assert_eq!(refinement.toc_count(), 0);
        assert_eq!(refinement.char_count, 8, "中文 2 + abc 3 + def 3");
        assert_eq!(refinement.encoding.as_deref(), Some("UTF-8"));
    }

    #[test]
    fn refine_txt_reports_detected_gbk_encoding() {
        // 短样本的编码探测本就不可靠，用长文本验证探测能力（与 perch-txt 的测试同理）
        let original = "中文测试，这是一段足够长的文本用来让编码探测器有足够的依据。".repeat(20);
        let (gbk_bytes, _, had_errors) = encoding_rs::GBK.encode(&original);
        assert!(!had_errors);

        let refinement = refine_txt(&gbk_bytes, ExtractOptions::default()).unwrap();
        assert!(
            matches!(refinement.encoding.as_deref(), Some("GBK" | "GB18030")),
            "应落在 GB 系编码，实际 {:?}",
            refinement.encoding
        );
        assert_eq!(refinement.char_count, count_visible_chars(&original));
    }

    // ---------- TXT：阶段③索引 + 定位 ----------

    #[test]
    fn txt_book_is_indexable_and_locates_by_char_offset() {
        let book = extract_txt(
            "春天的花开\r\n夏天的雨落".as_bytes(),
            ExtractOptions::default(),
        )
        .unwrap();
        let index = index_book(&book, 64);

        let hits = index.search("夏天的雨落");
        assert_eq!(hits.len(), 1, "TXT 书应能被 index_book 检索到");

        let hit = &hits[0];
        let locator = locate_hit(&book, hit).unwrap();
        assert_eq!(locator.href, "txt:", "TXT 命中不应给出 EPUB 章节 href");
        assert_eq!(locator.perch.format, Some(Format::Txt));
        assert_eq!(locator.perch.txt_offset, Some(hit.char_start));
        assert!(
            locator.perch.txt_byte_offset.is_some(),
            "应同时给出便于切片的字节偏移"
        );
        assert_eq!(locator.locations.partial_cfi, None);
        assert!(locator.locations.progression.is_some());
        assert_eq!(
            locator
                .text
                .as_ref()
                .and_then(|context| context.highlight.as_deref()),
            Some(hit.snippet.as_str())
        );
        // TXT 的 Locator 必须有 txtOffset 才能通过模型校验
        locator.validate().unwrap();
    }

    #[test]
    fn pdf_hit_has_no_locator() {
        let book = BookText {
            format: Format::Pdf,
            text: "x".to_owned(),
            chapters: Vec::new(),
            truncated: false,
        };
        let hit = Hit {
            char_start: 0,
            char_end: 1,
            chunk: 0,
            snippet: "x".to_owned(),
        };
        assert!(locate_hit(&book, &hit).is_none(), "PDF 不走这条定位链路");
    }

    #[test]
    fn book_text_serde_carries_format_and_defaults_old_json_to_epub() {
        let book = extract_txt("正文".as_bytes(), ExtractOptions::default()).unwrap();
        let json = serde_json::to_string(&book).unwrap();
        assert!(json.contains("\"format\":\"txt\""), "json = {json}");

        // 旧版本落盘的 BookText 没有 format 字段，反序列化时应按 EPUB 补齐
        let old = r#"{"text":"正文","chapters":[],"truncated":false}"#;
        let restored: BookText = serde_json::from_str(old).unwrap();
        assert_eq!(restored.format, Format::Epub);
    }
    // ---------- 格式嗅探与统一入口 ----------

    #[test]
    fn sniff_detects_epub_pdf_and_falls_back_to_txt() {
        assert_eq!(
            sniff_format(&build_epub(&["x"], false, false)),
            Format::Epub
        );
        assert_eq!(sniff_format(b"%PDF-1.7\n..."), Format::Pdf);
        assert_eq!(sniff_format("普通文本内容".as_bytes()), Format::Txt);
        // 空输入也按 TXT 处理（交给后续的空内容校验报错，而不是在这里猜）
        assert_eq!(sniff_format(b""), Format::Txt);
    }

    #[test]
    fn refine_dispatches_by_format() {
        let epub = build_epub(&["正文"], false, false);
        assert_eq!(
            refine(&epub, Format::Epub, ExtractOptions::default())
                .unwrap()
                .title
                .as_deref(),
            Some("测试书名")
        );

        let txt = refine(
            "中文纯文本".as_bytes(),
            Format::Txt,
            ExtractOptions::default(),
        )
        .unwrap();
        assert_eq!(
            txt.title, None,
            "TXT 没有内嵌元数据，标题由调用方从文件名取"
        );
        assert!(txt.char_count > 0);
    }

    #[test]
    fn pdf_is_rejected_with_a_stable_code() {
        // PDF 只做结构解析，不做正文抽取，因此不能进阶段②③
        for result in [
            refine(b"%PDF-1.7", Format::Pdf, ExtractOptions::default()).map(|_| ()),
            extract_text(b"%PDF-1.7", Format::Pdf, ExtractOptions::default()).map(|_| ()),
        ] {
            let error = result.unwrap_err();
            assert_eq!(error.code(), "JOBS_UNSUPPORTED_FORMAT");
            assert_eq!(error, JobsError::UnsupportedFormat { format: "pdf" });
        }
    }

    #[test]
    fn extract_text_dispatches_by_format() {
        let epub = build_epub(&["甲乙"], false, false);
        let from_epub = extract_text(&epub, Format::Epub, ExtractOptions::default()).unwrap();
        assert_eq!(from_epub.format, Format::Epub);

        let from_txt =
            extract_text("甲乙".as_bytes(), Format::Txt, ExtractOptions::default()).unwrap();
        assert_eq!(from_txt.format, Format::Txt);
        assert_eq!(from_txt.chapters.len(), 1);
        assert_eq!(from_txt.chapters[0].href, "txt:");
    }
}
