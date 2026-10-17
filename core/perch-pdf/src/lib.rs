//! PDF **结构**解析：页数、大纲、元数据。
//!
//! # 明确不做什么
//!
//! - **不渲染**：排版与显示交给平台（iOS 用 PDFKit / Android 用 PdfRenderer）
//! - **不抽正文**：文本抽取需要字体、编码、ToUnicode CMap 的全套支持，
//!   且与渲染结果的一致性无法在 Linux 上验证。按 `docs/02` 的决策，
//!   正文走**注入的 port**：核心只定义契约，平台实现。
//!
//! 本 crate 只回答"这本书有多少页、目录在哪、叫什么名字"——
//! 也就是书库和阅读器**在打开文件之前**就需要的信息，
//! 这类信息纯逻辑可解，因此能在 Linux 上完整测试。
//!
//! # 为什么进度表示是一页号而不是百分比
//!
//! PDF 的版面与设备无关（固定 A4/Letter 尺寸），页号是**稳定**的定位单位；
//! 百分比会随缩放/裁剪漂移。因此 [`Locator`] 对 PDF 的约定是
//! 「整个文档一个资源 `pdf:`，页号放在 `perch.pdf_page`」（见 `docs/02` §四）。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use lopdf::{Dictionary, Document, Object, ObjectId, Outline};
use perch_model::Locator;
use serde::{Deserialize, Serialize};

/// 大纲（目录）的一项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutlineItem {
    /// 标题。
    pub title: String,
    /// 跳转到的**页索引（0 基）**，与 `perch_model::Locator::perch.pdf_page` 同一约定。
    ///
    /// 目标解析不出来时为 `None`——**不猜**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_index: Option<u32>,
    /// 子项。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<OutlineItem>,
}

impl OutlineItem {
    /// 本项及其所有后代的数量。
    #[must_use]
    pub fn total_count(&self) -> usize {
        1 + self.children.iter().map(Self::total_count).sum::<usize>()
    }
}

/// PDF 文档的结构信息。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfInfo {
    /// 页数（至少 1）。
    pub page_count: u32,
    /// PDF 版本，如 `1.7`。
    pub version: String,
    /// 标题（来自 Info 字典，可能缺失）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 作者。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// 主题。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// 生成程序。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
    /// 大纲（顶层项）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outline: Vec<OutlineItem>,
}

impl PdfInfo {
    /// 大纲里所有条目的总数（含子项）。
    #[must_use]
    pub fn outline_count(&self) -> usize {
        self.outline.iter().map(OutlineItem::total_count).sum()
    }

    /// 为某一页构造 [`Locator`]。
    ///
    /// `page_index` 是**0 基页索引**（与 `Locator::perch.pdf_page` 一致）。
    /// 越界时返回 `None`——宁可让调用方发现，也不要静默夹到边界。
    #[must_use]
    pub fn locator_for(&self, page_index: u32) -> Option<Locator> {
        if page_index >= self.page_count {
            return None;
        }
        Some(Locator::new_pdf(page_index, Some(self.page_count)))
    }

    /// 把页索引换算成 0.0~1.0 的进度（`page_index` 为 0 基）。
    ///
    /// 公式与 `Locator::new_pdf` **完全一致**（`index / (总页数 - 1)`）：
    /// 首页是 0.0、末页是 1.0。两处算法必须同源，否则同一页会算出两个进度。
    /// 用于"读了 37%"这类展示；**定位仍然用页索引**。
    #[must_use]
    pub fn progression_of(&self, page_index: u32) -> Option<f64> {
        self.locator_for(page_index)
            .and_then(|locator| locator.locations.progression)
    }
}

/// 从 [`Locator`] 取 **0 基页索引**。不是 PDF 定位时返回 `None`。
#[must_use]
pub fn page_index_of(locator: &Locator) -> Option<u32> {
    locator.perch.pdf_page
}

/// PDF 解析错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PdfError {
    /// 输入为空。
    #[error("PDF 内容为空")]
    Empty,
    /// 文件头不是 `%PDF-`。
    #[error("不是 PDF 文件（缺少 %PDF- 文件头）")]
    NotPdf,
    /// 文档被加密，需要密码才能处理。
    ///
    /// 与"损坏"**必须分开报**：损坏意味着数据有问题，
    /// 加密意味着文件没问题、只是我们无权读取，处理方式完全不同。
    #[error("PDF 已加密，需要密码才能解析")]
    Encrypted,
    /// 文件里一页都没有。
    #[error("PDF 里没有任何页面")]
    NoPages,
    /// 结构损坏，无法解析。
    #[error("PDF 结构损坏：{reason}")]
    Corrupted {
        /// 底层原因。
        reason: String,
    },
}

impl PdfError {
    /// 稳定的错误码字符串。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Empty => "PDF_EMPTY",
            Self::NotPdf => "PDF_NOT_PDF",
            Self::Encrypted => "PDF_ENCRYPTED",
            Self::NoPages => "PDF_NO_PAGES",
            Self::Corrupted { .. } => "PDF_CORRUPTED",
        }
    }
}

/// 解析 PDF 结构（页数 / 大纲 / 元数据）。
///
/// # Errors
///
/// - [`PdfError::Empty`]：输入为空
/// - [`PdfError::NotPdf`]：没有 `%PDF-` 文件头（快速否定，不必进入完整解析）
/// - [`PdfError::Encrypted`]：文档加密
/// - [`PdfError::NoPages`]：解析成功但没有页面
/// - [`PdfError::Corrupted`]：结构损坏
pub fn analyze(bytes: &[u8]) -> Result<PdfInfo, PdfError> {
    if bytes.is_empty() {
        return Err(PdfError::Empty);
    }
    // 规范允许文件头前有少量垃圾字节，所以只在**前 1KB** 里找 %PDF-
    if !contains(&bytes[..bytes.len().min(1024)], b"%PDF-") {
        return Err(PdfError::NotPdf);
    }

    let document = Document::load_mem(bytes).map_err(|error| {
        // 加密文档在给出口令之前往往加载失败。用 "/Encrypt" 作为判据区分
        // 「没权限读」和「文件坏了」——两者的产品处理完全不同。
        if contains(bytes, b"/Encrypt") {
            PdfError::Encrypted
        } else {
            PdfError::Corrupted {
                reason: error.to_string(),
            }
        }
    })?;

    if document.is_encrypted() {
        return Err(PdfError::Encrypted);
    }

    let pages = document.get_pages();
    let Ok(page_count) = u32::try_from(pages.len()) else {
        return Err(PdfError::Corrupted {
            reason: "页数超出 u32 范围".to_owned(),
        });
    };
    if page_count == 0 {
        return Err(PdfError::NoPages);
    }

    // 反查表：页面对象 id → 页号，供大纲目标解析
    let mut id_to_page: BTreeMap<ObjectId, u32> = BTreeMap::new();
    for (number, id) in &pages {
        id_to_page.insert(*id, *number);
    }

    let info = info_dict(&document);
    Ok(PdfInfo {
        page_count,
        version: document.version.clone(),
        title: info.and_then(|dict| dict_text(&document, dict, b"Title")),
        author: info.and_then(|dict| dict_text(&document, dict, b"Author")),
        subject: info.and_then(|dict| dict_text(&document, dict, b"Subject")),
        producer: info.and_then(|dict| dict_text(&document, dict, b"Producer")),
        outline: extract_outline(&document, &id_to_page),
    })
}

/// 是否是 PDF（只看文件头，不解析结构）。
#[must_use]
pub fn looks_like_pdf(bytes: &[u8]) -> bool {
    contains(&bytes[..bytes.len().min(1024)], b"%PDF-")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn info_dict(document: &Document) -> Option<&Dictionary> {
    match document.trailer.get(b"Info").ok()? {
        Object::Reference(id) => document.get_dictionary(*id).ok(),
        Object::Dictionary(dict) => Some(dict),
        _ => None,
    }
}

fn dict_text(document: &Document, dict: &Dictionary, key: &[u8]) -> Option<String> {
    let object = dict.get(key).ok()?;
    // 值可能是直接对象，也可能是个引用
    let object = match object {
        Object::Reference(id) => document.get_object(*id).ok()?,
        other => other,
    };
    let bytes = match object {
        Object::String(bytes, _) => bytes,
        // 严格说标题必须是文本字符串；但解析畸形文件宜宽进，
        // 有些生成器会把标题写成 Name 对象。
        Object::Name(bytes) => bytes,
        _ => return None,
    };
    let text = decode_pdf_text(bytes)?;
    let trimmed = text.trim_matches(char::from(0)).trim().to_owned();
    // 空标题按"没有"处理：不要给 UI 一个空字符串去显示
    (!trimmed.is_empty()).then_some(trimmed)
}

/// 解码 PDF 文本字符串。
///
/// 规范里文本串有两种编码：
/// - 以 `FE FF` 开头的 UTF-16BE
/// - 其余按 PDFDocEncoding（与 Latin-1 在 ASCII 范围内一致）
///
/// 实践中还有第三类：**直接塞 UTF-8**（很多生成器这么干）。
/// 因此这里先试 UTF-8，失败再退回 Latin-1 —— 这是个近似，
/// 但比"只认规范的 PDFDocEncoding"能多读出真实的标题。
fn decode_pdf_text(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, false);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Some(text.to_owned());
    }
    // Latin-1 兜底：每个字节映射到同码位
    Some(bytes.iter().map(|&byte| char::from(byte)).collect())
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> Option<String> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| {
            if big_endian {
                u16::from_be_bytes([pair[0], pair[1]])
            } else {
                u16::from_le_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    String::from_utf16(&units).ok()
}

/// 把 lopdf 的扁平大纲列表转成树。
///
/// lopdf 的结构是「`Destination` 后面紧跟一个 `SubOutlines` 表示它的子项」，
/// 也就是说子项列表**不是**嵌在父项里的。所以这里用「挂到上一个条目」的方式还原。
fn extract_outline(document: &Document, id_to_page: &BTreeMap<ObjectId, u32>) -> Vec<OutlineItem> {
    let mut named_destinations = indexmap::IndexMap::new();
    // 大纲是**可选**的：解析失败就当没有目录，绝不能因此让整本书打不开
    let Ok(Some(items)) = document.get_outlines(None, None, &mut named_destinations) else {
        return Vec::new();
    };
    convert_outline(items, id_to_page)
}

fn convert_outline(items: Vec<Outline>, id_to_page: &BTreeMap<ObjectId, u32>) -> Vec<OutlineItem> {
    let mut converted: Vec<OutlineItem> = Vec::new();
    for item in items {
        match item {
            Outline::Destination(destination) => {
                let title = destination
                    .title()
                    .ok()
                    .and_then(object_text)
                    .unwrap_or_default();
                let page = destination
                    .page()
                    .ok()
                    .and_then(|object| resolve_page(object, id_to_page));
                converted.push(OutlineItem {
                    title,
                    page_index: page,
                    children: Vec::new(),
                });
            }
            Outline::SubOutlines(children) => {
                let kids = convert_outline(children, id_to_page);
                if kids.is_empty() {
                    continue;
                }
                match converted.last_mut() {
                    // 正常情形：子项列表属于紧邻的上一个条目
                    Some(parent) => parent.children = kids,
                    // 异常情形：没有父项。提升为同级，宁可层级错也不丢数据
                    None => converted.extend(kids),
                }
            }
        }
    }
    converted
}

fn object_text(object: &Object) -> Option<String> {
    let Object::String(bytes, _) = object else {
        return None;
    };
    let text = decode_pdf_text(bytes)?;
    let trimmed = text.trim_matches(char::from(0)).trim().to_owned();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// 解析大纲目标为**0 基页索引**。
///
/// lopdf 的 `get_pages()` 键是**1 基**页号，GoToR 的页序号也是 1 基，
/// 所以这里统一减一。整个 crate 对外只有 0 基一种约定（见模块文档）。
fn resolve_page(object: &Object, id_to_page: &BTreeMap<ObjectId, u32>) -> Option<u32> {
    let one_based = match object {
        Object::Reference(id) => id_to_page.get(id).copied(),
        Object::Integer(number) => u32::try_from(*number).ok(),
        _ => None,
    }?;
    one_based.checked_sub(1)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use lopdf::dictionary;

    /// 用 lopdf 造一个最小可解析的 PDF：`pages` 页，带 Info 字典。
    fn build_pdf(pages: usize, title: Option<&str>) -> Vec<u8> {
        let mut document = Document::with_version("1.7");
        let pages_id = document.new_object_id();
        let mut page_ids = Vec::new();
        for _ in 0..pages {
            let contents_id =
                document.add_object(lopdf::Stream::new(Dictionary::new(), Vec::new()));
            let page_id = document.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
                "Contents" => contents_id,
            });
            page_ids.push(page_id);
        }
        let kids: Vec<Object> = page_ids.iter().map(|id| Object::Reference(*id)).collect();
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => i64::try_from(pages).unwrap(),
            }),
        );
        let catalog = dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        };
        if let Some(title) = title {
            // ⚠️ 必须用 string_literal：dictionary! 里的 &str 会变成 Name 对象，
            // 而真实 PDF 的标题是 (...) 文本字符串。写错了会得到"读不到标题"的假象。
            let info_id = document.add_object(dictionary! {
                "Title" => Object::string_literal(title),
            });
            document.trailer.set("Info", info_id);
        }
        let catalog_id = document.add_object(catalog);
        document.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    /// 造一个带两级目录的 PDF：
    ///
    /// ```text
    /// 第一章  → 第 0 页
    ///   1.1 节 → 第 1 页
    /// 第二章  → 第 2 页
    /// ```
    fn build_pdf_with_outline() -> Vec<u8> {
        let mut document = Document::with_version("1.7");
        let pages_id = document.new_object_id();
        let mut page_ids = Vec::new();
        for _ in 0..3 {
            let contents_id =
                document.add_object(lopdf::Stream::new(Dictionary::new(), Vec::new()));
            let page_id = document.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
                "Contents" => contents_id,
            });
            page_ids.push(page_id);
        }
        let kids: Vec<Object> = page_ids.iter().map(|id| Object::Reference(*id)).collect();
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => 3_i64,
            }),
        );

        let outlines_id = document.new_object_id();
        let chapter1_id = document.new_object_id();
        let section_id = document.new_object_id();
        let chapter2_id = document.new_object_id();

        let dest = |page: ObjectId| -> Object {
            Object::Array(vec![Object::Reference(page), Object::Name(b"Fit".to_vec())])
        };

        document.objects.insert(
            chapter1_id,
            Object::Dictionary(dictionary! {
                "Title" => Object::string_literal("第一章"),
                "Parent" => outlines_id,
                "Dest" => dest(page_ids[0]),
                "Next" => chapter2_id,
                "First" => section_id,
                "Count" => 1_i64,
            }),
        );
        document.objects.insert(
            section_id,
            Object::Dictionary(dictionary! {
                "Title" => Object::string_literal("1.1 节"),
                "Parent" => chapter1_id,
                "Dest" => dest(page_ids[1]),
            }),
        );
        document.objects.insert(
            chapter2_id,
            Object::Dictionary(dictionary! {
                "Title" => Object::string_literal("第二章"),
                "Parent" => outlines_id,
                "Dest" => dest(page_ids[2]),
                "Prev" => chapter1_id,
            }),
        );
        document.objects.insert(
            outlines_id,
            Object::Dictionary(dictionary! {
                "Type" => "Outlines",
                "First" => chapter1_id,
                "Last" => chapter2_id,
                "Count" => 3_i64,
            }),
        );

        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "Outlines" => outlines_id,
        });
        document.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    // ---------- 大纲 ----------

    #[test]
    fn outline_tree_is_rebuilt_from_flat_list() {
        let info = analyze(&build_pdf_with_outline()).unwrap();
        assert_eq!(info.page_count, 3);
        assert_eq!(info.outline.len(), 2, "顶层应当有 2 章");
        assert_eq!(info.outline[0].title, "第一章");
        assert_eq!(info.outline[0].page_index, Some(0), "0 基页索引");
        assert_eq!(info.outline[1].title, "第二章");
        assert_eq!(info.outline[1].page_index, Some(2));

        // 子目录必须挂在第一章下，而不是变成同级
        assert_eq!(info.outline[0].children.len(), 1);
        assert_eq!(info.outline[0].children[0].title, "1.1 节");
        assert_eq!(info.outline[0].children[0].page_index, Some(1));
        assert!(info.outline[1].children.is_empty());

        assert_eq!(info.outline_count(), 3);
    }

    #[test]
    fn outline_item_total_count_includes_descendants() {
        let info = analyze(&build_pdf_with_outline()).unwrap();
        assert_eq!(info.outline[0].total_count(), 2);
        assert_eq!(info.outline[1].total_count(), 1);
    }

    #[test]
    fn outline_pages_round_trip_through_locator() {
        let info = analyze(&build_pdf_with_outline()).unwrap();
        for item in &info.outline {
            let index = item.page_index.unwrap();
            let locator = info.locator_for(index).unwrap();
            assert_eq!(page_index_of(&locator), Some(index));
        }
    }

    #[test]
    fn pdf_without_outline_has_empty_tree() {
        let info = analyze(&build_pdf(2, None)).unwrap();
        assert!(info.outline.is_empty());
        assert_eq!(info.outline_count(), 0);
    }

    // ---------- 基本结构 ----------

    #[test]
    fn reads_page_count_and_version() {
        let bytes = build_pdf(3, None);
        let info = analyze(&bytes).unwrap();
        assert_eq!(info.page_count, 3);
        assert_eq!(info.version, "1.7");
        assert!(info.outline.is_empty());
        assert_eq!(info.outline_count(), 0);
    }

    #[test]
    fn reads_title_from_info_dict() {
        let bytes = build_pdf(1, Some("三体"));
        let info = analyze(&bytes).unwrap();
        assert_eq!(info.title.as_deref(), Some("三体"));
    }

    #[test]
    fn missing_metadata_is_none_not_empty_string() {
        let bytes = build_pdf(1, None);
        let info = analyze(&bytes).unwrap();
        assert_eq!(info.title, None);
        assert_eq!(info.author, None);
    }

    // ---------- 拒绝非 PDF / 坏输入 ----------

    #[test]
    fn empty_input_is_rejected() {
        assert_eq!(analyze(&[]).unwrap_err().code(), "PDF_EMPTY");
    }

    #[test]
    fn non_pdf_is_rejected() {
        assert_eq!(
            analyze(b"hello world, definitely not a pdf")
                .unwrap_err()
                .code(),
            "PDF_NOT_PDF"
        );
    }

    #[test]
    fn truncated_pdf_does_not_panic() {
        let bytes = build_pdf(2, Some("截断测试"));
        // 从各个长度截断：都必须返回错误或成功，但**不能 panic**
        for cut in [5, 10, 50, 100, bytes.len() / 2, bytes.len() - 1] {
            let _ = analyze(&bytes[..cut.min(bytes.len())]);
        }
    }

    #[test]
    fn garbage_after_header_does_not_panic() {
        let mut bytes = b"%PDF-1.7\n".to_vec();
        bytes.extend(std::iter::repeat_n(0x41_u8, 500));
        let result = analyze(&bytes);
        assert!(result.is_err(), "垃圾内容应当报错");
    }

    #[test]
    fn encrypted_marker_is_reported_as_encrypted() {
        // 有 %PDF- 头 + /Encrypt 标记但结构坏：必须报"加密"而不是"损坏"
        let mut bytes = b"%PDF-1.7\n".to_vec();
        bytes.extend(std::iter::repeat_n(0x00_u8, 100));
        bytes.extend_from_slice(b"/Encrypt");
        bytes.extend(std::iter::repeat_n(0x00_u8, 100));
        assert_eq!(analyze(&bytes).unwrap_err().code(), "PDF_ENCRYPTED");
    }

    #[test]
    fn header_may_be_preceded_by_junk_within_1kb() {
        let inner = build_pdf(1, None);
        let mut bytes = vec![0x20_u8; 100];
        bytes.extend_from_slice(&inner);
        assert!(analyze(&bytes).is_ok());
        assert!(looks_like_pdf(&bytes));
    }

    // ---------- Locator / 进度 ----------

    #[test]
    fn locator_for_page_uses_pdf_contract() {
        let bytes = build_pdf(10, None);
        let info = analyze(&bytes).unwrap();
        let locator = info.locator_for(3).unwrap();
        // 文档是一个资源，页索引在 perch.pdf_page
        assert_eq!(locator.href, "pdf:");
        assert_eq!(locator.perch.pdf_page, Some(3));
        assert_eq!(page_index_of(&locator), Some(3));
        // progression 与 new_pdf 同源：index/(total-1) = 3/9
        assert!((locator.locations.progression.unwrap() - 3.0 / 9.0).abs() < 1e-9);
    }

    #[test]
    fn out_of_range_page_is_rejected_not_clamped() {
        let bytes = build_pdf(5, None);
        let info = analyze(&bytes).unwrap();
        assert!(info.locator_for(5).is_none(), "0 基下 5 已越界");
        assert!(info.locator_for(100).is_none());
        assert!(info.progression_of(5).is_none());
        // 边界值仍然可用：0 和 count-1
        assert!(info.locator_for(0).is_some());
        assert!(info.locator_for(4).is_some());
    }

    #[test]
    fn progression_is_page_over_total() {
        let bytes = build_pdf(4, None);
        let info = analyze(&bytes).unwrap();
        // 首页 0.0，末页 1.0
        assert_eq!(info.progression_of(0), Some(0.0));
        assert_eq!(info.progression_of(3), Some(1.0));
        assert_eq!(info.progression_of(1), Some(1.0 / 3.0));
    }

    // ---------- 文本解码 ----------

    #[test]
    fn utf16be_titles_are_decoded() {
        // "书" = U+4E66
        let bytes = [0xFE, 0xFF, 0x4E, 0x66];
        assert_eq!(decode_pdf_text(&bytes).as_deref(), Some("书"));
    }

    #[test]
    fn utf8_titles_are_accepted() {
        // 现实中很多生成器直接写 UTF-8，虽然不完全合规
        assert_eq!(
            decode_pdf_text("第一章".as_bytes()).as_deref(),
            Some("第一章")
        );
    }

    #[test]
    fn latin1_fallback_never_fails() {
        // 0xE9 在 Latin-1 里是 é，不是合法 UTF-8 单字节
        assert_eq!(decode_pdf_text(&[0xE9]).as_deref(), Some("é"));
    }

    #[test]
    fn trailing_nul_bytes_are_trimmed() {
        assert_eq!(decode_pdf_text(b"Title\x00").as_deref(), Some("Title\u{0}"),);
    }
}
