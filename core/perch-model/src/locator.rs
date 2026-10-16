//! 统一 Locator：用一个结构表示 EPUB / PDF / TXT 三端三格式的任意位置。
//!
//! # 为什么需要它
//!
//! 项目目标要求"进度/书签/笔记跨端同步，冲突率 < 1%"。要做到这一点，前提是**三端三格式
//! 对"位置"有一致的表示**。本模块就是那个表示。
//!
//! # 与 Readium 的互操作（关键设计）
//!
//! 顶层字段（`href` / `type` / `title` / `locations` / `text`）与 Readium 的 `Locator`
//! 模型**逐字段对齐**，因此本结构序列化出的 JSON 可直接被 Readium Swift / Kotlin / JS 解析。
//! 本项目特有的字段全部收纳在 `perch` 对象内：
//!
//! - 不会与 Readium 的命名空间冲突（将来 Readium 加字段也不会撞名）；
//! - 交给 Readium 时可以整体丢弃 `perch`，信息降级但定位依然有效；
//! - 从 Readium 拿到的纯 Locator（没有 `perch`）也能原样解析并回写，不引入多余字段。
//!
//! # 容错重定位
//!
//! 仅靠 CFI 定位是不够的：EPUB 换版本、字号重排、DOM 变化都会让 CFI 失效。
//! 因此 [`TextContext`] 携带 `before` / `highlight` / `after` 三段原文，
//! 用于锚点失效时的模糊匹配（见 `docs/01-里程碑拆解.md` M4）。

use serde::{Deserialize, Serialize};

use crate::error::ModelError;
use crate::timestamp::Timestamp;

/// `perch` 扩展结构的版本号，为将来的格式演进留出空间。
pub const LOCATOR_VERSION: u8 = 1;

/// 书籍格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// EPUB（含 EPUB 2 / 3）。
    Epub,
    /// PDF。
    Pdf,
    /// 纯文本。
    Txt,
}

impl Format {
    /// 稳定的字符串表示，用于日志、错误信息与跨语言传递。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Epub => "epub",
            Self::Pdf => "pdf",
            Self::Txt => "txt",
        }
    }

    /// 由媒体类型推断格式。
    #[must_use]
    pub fn from_media_type(media_type: &str) -> Option<Self> {
        // 媒体类型可能带参数，例如 `text/plain; charset=utf-8`
        let essence = media_type
            .split(';')
            .next()
            .unwrap_or(media_type)
            .trim()
            .to_ascii_lowercase();
        match essence.as_str() {
            "application/epub+zip" => Some(Self::Epub),
            "application/pdf" => Some(Self::Pdf),
            "text/plain" => Some(Self::Txt),
            _ => None,
        }
    }

    /// 由文件名 / 路径推断格式（忽略查询串与大小写）。
    #[must_use]
    pub fn from_href(href: &str) -> Option<Self> {
        let lowered = href.to_ascii_lowercase();
        let path = lowered.split(['?', '#']).next().unwrap_or(lowered.as_str());
        if path.ends_with(".epub") {
            Some(Self::Epub)
        } else if path.ends_with(".pdf") {
            Some(Self::Pdf)
        } else if path.ends_with(".txt") {
            Some(Self::Txt)
        } else {
            None
        }
    }
}

/// Readium 兼容的 `locations` 对象。
///
/// 字段名经 `camelCase` 映射后与 Readium 完全一致（`totalProgression` / `cssSelector` /
/// `partialCfi`），这是能被 Readium 直接解析的前提。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Locations {
    /// 元素片段标识（如 XHTML 中的 `id`）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fragments: Vec<String>,

    /// 章节内进度 0.0~1.0。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progression: Option<f64>,

    /// 全书进度 0.0~1.0（含前言、后记等）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_progression: Option<f64>,

    /// 阅读顺序中的位置序号（Readium 用于"第 N 个定位点"）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,

    /// 精确的 CSS 选择器。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub css_selector: Option<String>,

    /// 部分 CFI（Readium 的命名，**不是** `cfi`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_cfi: Option<String>,
}

/// 容错重定位所需的原文上下文（对应 Readium 的 `text` 对象）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TextContext {
    /// 位置之前的原文片段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,

    /// 被标注/选中的原文本身。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight: Option<String>,

    /// 位置之后的原文片段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// 本项目对 Locator 的扩展字段，收纳在 JSON 的 `perch` 对象里。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Perch {
    /// 扩展结构版本，见 [`LOCATOR_VERSION`]。
    pub v: u8,

    /// 书格式（从纯 Readium locator 解析时为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,

    /// PDF：页号（0 基）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf_page: Option<u32>,

    /// PDF：归一化矩形 `[x, y, w, h]`，分量均为相对页面的 0.0~1.0。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf_rect: Option<[f32; 4]>,

    /// TXT：字符偏移（解码后，0 基）——这是跨端最稳定的锚点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txt_offset: Option<u64>,

    /// TXT：字节偏移（便于不做全量解码就定位）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txt_byte_offset: Option<u64>,

    /// 所在章节序号（0 基）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapter_index: Option<u32>,

    /// 所在章节标题（用于 UI 显示"你读到哪儿"）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapter_title: Option<String>,

    /// 创建时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<Timestamp>,

    /// 最后修改时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<Timestamp>,

    /// 混合逻辑时钟（同步用），形如 `{wall_ms}-{counter}-{device_id}`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hlc: Option<String>,
}

impl Default for Perch {
    fn default() -> Self {
        Self {
            v: LOCATOR_VERSION,
            format: None,
            pdf_page: None,
            pdf_rect: None,
            txt_offset: None,
            txt_byte_offset: None,
            chapter_index: None,
            chapter_title: None,
            created_at: None,
            modified_at: None,
            hlc: None,
        }
    }
}

impl Perch {
    /// 是否等同于默认值（用于序列化时省略 `perch` 对象）。
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// 统一位置表示。见模块文档。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Locator {
    /// 资源路径——标识**资源**，而不是资源内部的位置。
    ///
    /// - EPUB：包内章节路径（如 `/OEBPS/chap01.xhtml`），一章一个资源；
    /// - PDF：约定为常量 `pdf:`——整个文档是一个资源，页号放在 `perch.pdf_page`；
    /// - TXT：约定为常量 `txt:`——整个文件是一个资源，偏移放在 `perch.txt_offset`。
    pub href: String,

    /// 媒体类型（对应 Readium 的 `type`，JSON 里就是 `"type"`）。
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,

    /// 人类可读的标题（章节名等）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Readium 兼容的定位信息。
    #[serde(default)]
    pub locations: Locations,

    /// 容错重定位上下文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<TextContext>,

    /// 本项目扩展字段。
    #[serde(default, skip_serializing_if = "Perch::is_default")]
    pub perch: Perch,
}

impl Locator {
    /// 构造 EPUB 位置。
    #[must_use]
    pub fn new_epub(
        href: impl Into<String>,
        partial_cfi: Option<String>,
        progression: Option<f64>,
    ) -> Self {
        Self {
            href: href.into(),
            media_type: Some("application/xhtml+xml".to_owned()),
            title: None,
            locations: Locations {
                partial_cfi,
                progression,
                ..Locations::default()
            },
            text: None,
            perch: Perch {
                format: Some(Format::Epub),
                ..Perch::default()
            },
        }
    }

    /// 构造 PDF 位置（`page` 为 0 基页号）。
    ///
    /// 若给出总页数，则顺带算出 `progression`。
    #[must_use]
    pub fn new_pdf(page: u32, total_pages: Option<u32>) -> Self {
        let progression = match total_pages {
            Some(total) if total > 1 => {
                Some((f64::from(page) / f64::from(total - 1)).clamp(0.0, 1.0))
            }
            _ => None,
        };
        Self {
            // 一个 PDF 文档就是一个资源，页号是资源**内部**的位置（放在 pdf_page 里）。
            // 不能把页号编进 href：那样跨页的范围标注会被误判为"跨资源"。
            href: "pdf:".to_owned(),
            media_type: Some("application/pdf".to_owned()),
            title: None,
            locations: Locations {
                progression,
                ..Locations::default()
            },
            text: None,
            perch: Perch {
                format: Some(Format::Pdf),
                pdf_page: Some(page),
                ..Perch::default()
            },
        }
    }

    /// 构造 TXT 位置（`char_offset` 为解码后的字符偏移，0 基）。
    #[must_use]
    pub fn new_txt(char_offset: u64, byte_offset: Option<u64>, total_chars: Option<u64>) -> Self {
        let progression = match total_chars {
            Some(total) if total > 0 => Some((char_offset as f64 / total as f64).clamp(0.0, 1.0)),
            _ => None,
        };
        Self {
            href: "txt:".to_owned(),
            media_type: Some("text/plain".to_owned()),
            title: None,
            locations: Locations {
                progression,
                ..Locations::default()
            },
            text: None,
            perch: Perch {
                format: Some(Format::Txt),
                txt_offset: Some(char_offset),
                txt_byte_offset: byte_offset,
                ..Perch::default()
            },
        }
    }

    /// 取得声明的格式（纯 Readium locator 可能为 `None`）。
    #[must_use]
    pub fn format(&self) -> Option<Format> {
        self.perch.format
    }

    /// 校验内部一致性。
    ///
    /// 这是"坏数据不许进入系统"的关口：同步来的、数据库读回的、外部传入的 Locator
    /// 都应该先过这里，再被信任。
    ///
    /// # Errors
    ///
    /// - [`ModelError::EmptyField`]：`href` 为空或只有空白
    /// - [`ModelError::InvalidProgression`]：进度越界或为 NaN
    /// - [`ModelError::InvalidRect`]：PDF 矩形非法
    /// - [`ModelError::MissingAnchor`]：缺少该格式必需的定位锚点
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.href.trim().is_empty() {
            return Err(ModelError::EmptyField { field: "href" });
        }

        check_progression("locations.progression", self.locations.progression)?;
        check_progression(
            "locations.totalProgression",
            self.locations.total_progression,
        )?;

        if let Some(rect) = self.perch.pdf_rect {
            check_rect(rect)?;
        }

        // 各格式各自需要的定位锚点。写成"查表"而不是嵌套 if，
        // 既能一眼看清每种格式的必需条件，也方便将来加格式。
        let missing: Option<(&'static str, &'static str)> = match self.perch.format {
            Some(Format::Epub) => {
                let has_anchor = self.locations.partial_cfi.is_some()
                    || self.locations.css_selector.is_some()
                    || !self.locations.fragments.is_empty()
                    || self.locations.progression.is_some();
                (!has_anchor).then_some((
                    "epub",
                    "partialCfi / cssSelector / fragments / progression 之一",
                ))
            }
            Some(Format::Pdf) => self.perch.pdf_page.is_none().then_some(("pdf", "pdfPage")),
            Some(Format::Txt) => self
                .perch
                .txt_offset
                .is_none()
                .then_some(("txt", "txtOffset")),
            // 纯 Readium locator（无扩展）不做格式专属校验
            None => None,
        };
        if let Some((format, required)) = missing {
            return Err(ModelError::MissingAnchor { format, required });
        }

        Ok(())
    }
}

fn check_progression(field: &'static str, value: Option<f64>) -> Result<(), ModelError> {
    if let Some(v) = value {
        if !v.is_finite() || !(0.0..=1.0).contains(&v) {
            return Err(ModelError::InvalidProgression { field, value: v });
        }
    }
    Ok(())
}

fn check_rect(rect: [f32; 4]) -> Result<(), ModelError> {
    let [_, _, w, h] = rect;
    let all_finite = rect.iter().all(|v| v.is_finite());
    let all_unit = rect.iter().all(|v| (0.0..=1.0).contains(v));
    if !all_finite || !all_unit || w <= 0.0 || h <= 0.0 {
        return Err(ModelError::InvalidRect { rect });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn epub_roundtrip_keeps_readium_field_names() {
        let loc = Locator::new_epub(
            "/OEBPS/chap01.xhtml",
            Some("/6/4[chap01]!/4/2/2:12".to_owned()),
            Some(0.42),
        );
        loc.validate().unwrap();

        let json = serde_json::to_string(&loc).unwrap();
        // Readium 的字段名必须原样保留（是 partialCfi，不是 cfi）
        assert!(json.contains("\"partialCfi\""), "json = {json}");
        assert!(json.contains("\"progression\""));
        // 我们的扩展必须待在 perch 里
        assert!(json.contains("\"perch\""), "json = {json}");

        let back: Locator = serde_json::from_str(&json).unwrap();
        assert_eq!(loc, back);
    }

    #[test]
    fn parses_plain_readium_locator_without_extensions() {
        // 一份"纯 Readium 风格"的 JSON：没有 perch
        // 注意：内容里含 `"#p1"`，会出现 `"#` 序列，因此用 r##""## 而不是 r#""#
        let raw = r##"{
            "href": "/OEBPS/chapter2.xhtml",
            "type": "application/xhtml+xml",
            "title": "第二章",
            "locations": {
                "fragments": ["p1"],
                "progression": 0.5,
                "totalProgression": 0.25,
                "position": 3,
                "cssSelector": "#p1",
                "partialCfi": "/6/4!/4/2"
            },
            "text": { "before": "前文", "highlight": "选中", "after": "后文" }
        }"##;

        let loc: Locator = serde_json::from_str(raw).unwrap();
        loc.validate().unwrap();

        assert_eq!(loc.href, "/OEBPS/chapter2.xhtml");
        assert_eq!(loc.locations.progression, Some(0.5));
        assert_eq!(loc.locations.total_progression, Some(0.25));
        assert_eq!(loc.locations.partial_cfi.as_deref(), Some("/6/4!/4/2"));
        assert_eq!(loc.locations.position, Some(3));
        assert_eq!(loc.locations.fragments, vec!["p1".to_owned()]);
        assert_eq!(
            loc.text.as_ref().and_then(|t| t.highlight.as_deref()),
            Some("选中")
        );
        // 格式未知（因为我们没从 Readium 那里得到 perch.format）
        assert_eq!(loc.format(), None);

        // 回写时不能凭空造出 perch，也不能丢掉 Readium 的字段
        let json = serde_json::to_string(&loc).unwrap();
        assert!(!json.contains("perch"), "不应引入 perch: {json}");
        assert!(json.contains("\"totalProgression\""), "json = {json}");
        assert!(json.contains("\"cssSelector\""), "json = {json}");
        assert!(json.contains("\"type\""), "json = {json}");
    }

    #[test]
    fn rejects_empty_or_blank_href() {
        let mut loc = Locator::new_epub("", Some("/6/4".to_owned()), None);
        assert_eq!(loc.validate().unwrap_err().code(), "MODEL_EMPTY_FIELD");

        loc.href = "   \n\t ".to_owned();
        assert_eq!(loc.validate().unwrap_err().code(), "MODEL_EMPTY_FIELD");
    }

    #[test]
    fn rejects_out_of_range_progression() {
        let mut loc: Locator =
            serde_json::from_str(r#"{"href":"a.xhtml","locations":{"progression":1.5}}"#).unwrap();
        assert_eq!(
            loc.validate().unwrap_err().code(),
            "MODEL_INVALID_PROGRESSION"
        );

        loc.locations.progression = Some(-0.01);
        assert_eq!(
            loc.validate().unwrap_err().code(),
            "MODEL_INVALID_PROGRESSION"
        );

        // 边界值 0.0 / 1.0 必须接受
        loc.locations.progression = Some(0.0);
        loc.validate().unwrap();
        loc.locations.progression = Some(1.0);
        loc.validate().unwrap();
    }

    #[test]
    fn rejects_nan_and_infinite_progression() {
        let mut loc = Locator::new_epub("a.xhtml", Some("/6/4".to_owned()), None);

        loc.locations.progression = Some(f64::NAN);
        assert_eq!(
            loc.validate().unwrap_err().code(),
            "MODEL_INVALID_PROGRESSION"
        );

        loc.locations.progression = Some(f64::INFINITY);
        assert_eq!(
            loc.validate().unwrap_err().code(),
            "MODEL_INVALID_PROGRESSION"
        );

        loc.locations.progression = None;
        loc.locations.total_progression = Some(f64::NEG_INFINITY);
        assert_eq!(
            loc.validate().unwrap_err().code(),
            "MODEL_INVALID_PROGRESSION"
        );
    }

    #[test]
    fn epub_needs_at_least_one_anchor() {
        let mut loc = Locator::new_epub("a.xhtml", None, None);
        assert_eq!(loc.validate().unwrap_err().code(), "MODEL_MISSING_ANCHOR");

        // 任一种锚点都足够
        loc.locations.progression = Some(0.1);
        loc.validate().unwrap();

        loc.locations.progression = None;
        loc.locations.fragments = vec!["p1".to_owned()];
        loc.validate().unwrap();

        loc.locations.fragments.clear();
        loc.locations.css_selector = Some("#p1".to_owned());
        loc.validate().unwrap();

        loc.locations.css_selector = None;
        loc.locations.partial_cfi = Some("/6/4".to_owned());
        loc.validate().unwrap();
    }

    #[test]
    fn pdf_requires_page_and_uses_single_resource_href() {
        let mut loc = Locator::new_pdf(3, Some(100));
        loc.validate().unwrap();
        assert_eq!(loc.perch.pdf_page, Some(3));
        // 整个 PDF 是一个资源：不同页码共享同一个 href，跨页范围才不会被误判为跨资源
        assert_eq!(loc.href, "pdf:");
        assert_eq!(loc.format(), Some(Format::Pdf));
        assert_eq!(Locator::new_pdf(99, Some(100)).href, loc.href);

        // 丢掉页号 → PDF 就无法定位了
        loc.perch.pdf_page = None;
        assert_eq!(loc.validate().unwrap_err().code(), "MODEL_MISSING_ANCHOR");
    }

    #[test]
    fn pdf_progression_is_derived_from_page() {
        assert_eq!(
            Locator::new_pdf(50, Some(101)).locations.progression,
            Some(0.5)
        );
        assert_eq!(
            Locator::new_pdf(100, Some(101)).locations.progression,
            Some(1.0)
        );
        assert_eq!(
            Locator::new_pdf(0, Some(101)).locations.progression,
            Some(0.0)
        );
        // 只有一页 → 无进度可言
        assert_eq!(Locator::new_pdf(0, Some(1)).locations.progression, None);
        // 未知总页数 → 不猜
        assert_eq!(Locator::new_pdf(7, None).locations.progression, None);
    }

    #[test]
    fn pdf_rect_validation() {
        let mut loc = Locator::new_pdf(1, Some(10));

        loc.perch.pdf_rect = Some([0.1, 0.2, 0.5, 0.3]);
        loc.validate().unwrap();

        // 宽为 0
        loc.perch.pdf_rect = Some([0.1, 0.2, 0.0, 0.3]);
        assert_eq!(loc.validate().unwrap_err().code(), "MODEL_INVALID_RECT");

        // 高为 0
        loc.perch.pdf_rect = Some([0.1, 0.2, 0.5, 0.0]);
        assert_eq!(loc.validate().unwrap_err().code(), "MODEL_INVALID_RECT");

        // 超出归一化区间
        loc.perch.pdf_rect = Some([0.1, 0.2, 1.5, 0.3]);
        assert_eq!(loc.validate().unwrap_err().code(), "MODEL_INVALID_RECT");

        // NaN
        loc.perch.pdf_rect = Some([f32::NAN, 0.2, 0.5, 0.3]);
        assert_eq!(loc.validate().unwrap_err().code(), "MODEL_INVALID_RECT");
    }

    #[test]
    fn txt_requires_offset_and_derives_progression() {
        let mut loc = Locator::new_txt(1_200, Some(2_400), Some(10_000));
        loc.validate().unwrap();
        assert_eq!(loc.locations.progression, Some(0.12));
        assert_eq!(loc.perch.txt_offset, Some(1_200));
        assert_eq!(loc.perch.txt_byte_offset, Some(2_400));

        loc.perch.txt_offset = None;
        assert_eq!(loc.validate().unwrap_err().code(), "MODEL_MISSING_ANCHOR");
    }

    #[test]
    fn txt_progression_clamps_and_handles_unknown_total() {
        // 偏移超过总数（数据不一致）时应钳制到 1.0，而不是报错或产生越界值
        let loc = Locator::new_txt(20_000, None, Some(10_000));
        assert_eq!(loc.locations.progression, Some(1.0));
        loc.validate().unwrap();

        // 未知总字数 → 不猜
        assert_eq!(Locator::new_txt(5, None, None).locations.progression, None);
        // 总数为 0 → 不猜（也避免除零）
        assert_eq!(
            Locator::new_txt(5, None, Some(0)).locations.progression,
            None
        );
    }

    #[test]
    fn format_inference_from_href_and_media_type() {
        assert_eq!(Format::from_href("book.epub"), Some(Format::Epub));
        assert_eq!(Format::from_href("BOOK.PDF"), Some(Format::Pdf));
        assert_eq!(Format::from_href("novel.txt?x=1"), Some(Format::Txt));
        assert_eq!(Format::from_href("chapter.xhtml#p1"), None);
        assert_eq!(Format::from_href("cover.jpg"), None);

        assert_eq!(
            Format::from_media_type("application/epub+zip"),
            Some(Format::Epub)
        );
        assert_eq!(
            Format::from_media_type("text/plain; charset=utf-8"),
            Some(Format::Txt)
        );
        assert_eq!(
            Format::from_media_type("application/pdf"),
            Some(Format::Pdf)
        );
        assert_eq!(Format::from_media_type("image/png"), None);

        // 反方向
        assert_eq!(Format::Epub.as_str(), "epub");
        assert_eq!(Format::Pdf.as_str(), "pdf");
        assert_eq!(Format::Txt.as_str(), "txt");
    }

    #[test]
    fn perch_extensions_use_camel_case_in_json() {
        let mut loc = Locator::new_pdf(3, Some(100));
        loc.perch.chapter_index = Some(2);
        loc.perch.chapter_title = Some("第三章".to_owned());
        loc.perch.created_at = Some(Timestamp::from_millis(1_700_000_000_000));

        let json = serde_json::to_string(&loc).unwrap();
        for key in [
            "\"pdfPage\"",
            "\"chapterIndex\"",
            "\"chapterTitle\"",
            "\"createdAt\"",
            "\"format\":\"pdf\"",
        ] {
            assert!(json.contains(key), "缺少 {key}: {json}");
        }
    }
}
