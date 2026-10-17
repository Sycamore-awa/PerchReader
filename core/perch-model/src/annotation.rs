//! 标注：高亮 / 下划线 / 笔记 / 书签。
//!
//! 同步设计要点（见 `docs/02-架构设计.md` §八）：
//!
//! - 每条标注有**独立 UUID**，按条合并，把冲突面缩到最小；
//! - 带 `deleted` **墓碑**——删除也必须同步，否则会在另一台设备上"复活"；
//! - 带 `hlc`（混合逻辑时钟），供字段级 LWW 排序，避免依赖设备本地时钟；
//! - **范围标注的两端作为一条记录整体合并**，不允许出现"起点是新的、终点是旧的"的撕裂状态。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::book::BookId;
use crate::error::ModelError;
use crate::locator::Locator;
use crate::timestamp::Timestamp;
use crate::util::is_false;

/// 标注唯一标识（UUID v4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AnnotationId(Uuid);

impl AnnotationId {
    /// 生成新标识。
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// 由已有 UUID 构造。
    #[must_use]
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// 取内部 UUID。
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// 从字符串解析。
    ///
    /// # Errors
    ///
    /// 字符串不是合法 UUID 时返回 [`ModelError::InvalidId`]。
    pub fn parse(value: &str) -> Result<Self, ModelError> {
        Uuid::parse_str(value)
            .map(Self)
            .map_err(|_| ModelError::InvalidId {
                kind: "annotation",
                value: value.to_owned(),
            })
    }
}

impl Default for AnnotationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AnnotationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 标注类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnnotationKind {
    /// 高亮（有范围）。
    Highlight,
    /// 下划线（有范围）。
    Underline,
    /// 笔记（点锚定或范围锚定，必须带文本）。
    Note,
    /// 书签（点锚定）。
    Bookmark,
}

impl AnnotationKind {
    /// 稳定的字符串表示。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Highlight => "highlight",
            Self::Underline => "underline",
            Self::Note => "note",
            Self::Bookmark => "bookmark",
        }
    }

    /// 是否为范围标注（需要结束位置）。
    #[must_use]
    pub const fn is_range(self) -> bool {
        matches!(self, Self::Highlight | Self::Underline)
    }

    /// 是否必须有笔记文本。
    #[must_use]
    pub const fn requires_note_text(self) -> bool {
        matches!(self, Self::Note)
    }
}

impl std::fmt::Display for AnnotationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条标注。
///
/// `start` 与 `end` 都是 [`Locator`]，因此单点定位与范围定位共用同一套重定位与同步逻辑
/// （见 `docs/02-架构设计.md` §四）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Annotation {
    /// 唯一标识。
    pub id: AnnotationId,
    /// 所属书籍。
    pub book_id: BookId,
    /// 类型。
    pub kind: AnnotationKind,
    /// 起始位置。
    pub start: Locator,
    /// 结束位置（点标注为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<Locator>,
    /// 颜色（十六进制，如 `#FFD400`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// 笔记正文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// 被标注的原文（冗余保存，用于列表展示与锚点失效后的重定位）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    /// 创建时间。
    pub created_at: Timestamp,
    /// 最后修改时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<Timestamp>,
    /// 软删除（墓碑）。
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
    /// 混合逻辑时钟，格式 `{wall_ms}-{counter}-{device_id}`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hlc: Option<String>,
}

impl Annotation {
    /// 构造一条高亮。
    #[must_use]
    pub fn new_highlight(
        book_id: BookId,
        start: Locator,
        end: Locator,
        color: Option<String>,
        excerpt: Option<String>,
    ) -> Self {
        Self {
            id: AnnotationId::new(),
            book_id,
            kind: AnnotationKind::Highlight,
            start,
            end: Some(end),
            color,
            note: None,
            excerpt,
            created_at: Timestamp::now(),
            modified_at: None,
            deleted: false,
            hlc: None,
        }
    }

    /// 构造一个书签（点锚定）。
    #[must_use]
    pub fn new_bookmark(book_id: BookId, at: Locator) -> Self {
        Self {
            id: AnnotationId::new(),
            book_id,
            kind: AnnotationKind::Bookmark,
            start: at,
            end: None,
            color: None,
            note: None,
            excerpt: None,
            created_at: Timestamp::now(),
            modified_at: None,
            deleted: false,
            hlc: None,
        }
    }

    /// 构造一条笔记（点锚定）。
    #[must_use]
    pub fn new_note(book_id: BookId, at: Locator, note: impl Into<String>) -> Self {
        Self {
            id: AnnotationId::new(),
            book_id,
            kind: AnnotationKind::Note,
            start: at,
            end: None,
            color: None,
            note: Some(note.into()),
            excerpt: None,
            created_at: Timestamp::now(),
            modified_at: None,
            deleted: false,
            hlc: None,
        }
    }

    /// 是否为范围标注。
    #[must_use]
    pub fn is_range(&self) -> bool {
        self.end.is_some()
    }

    /// 校验不变量。
    ///
    /// # Errors
    ///
    /// - [`ModelError::InvalidAnnotationShape`]：形状不合法（书签带了结束位置、
    ///   高亮缺少结束位置、笔记缺正文、范围跨资源）
    /// - [`ModelError::InvalidRange`]：结束位置早于起始位置
    /// - 以及 [`Locator::validate`] 可能返回的所有错误
    pub fn validate(&self) -> Result<(), ModelError> {
        self.start.validate()?;

        if let Some(end) = &self.end {
            end.validate()?;
        }

        match self.kind {
            AnnotationKind::Bookmark => {
                if self.end.is_some() {
                    return Err(ModelError::InvalidAnnotationShape {
                        kind: self.kind.as_str(),
                        detail: "书签是点锚定，不应带结束位置",
                    });
                }
            }
            AnnotationKind::Highlight | AnnotationKind::Underline => {
                let Some(end) = &self.end else {
                    return Err(ModelError::InvalidAnnotationShape {
                        kind: self.kind.as_str(),
                        detail: "范围标注必须有结束位置",
                    });
                };
                // v1 约束：范围必须落在同一个资源内。
                // 跨章节高亮需要拆成多条记录，留待后续版本。
                if end.href != self.start.href {
                    return Err(ModelError::InvalidAnnotationShape {
                        kind: self.kind.as_str(),
                        detail: "范围标注的两端必须在同一资源内",
                    });
                }
                if is_reversed(&self.start, end) {
                    return Err(ModelError::InvalidRange);
                }
            }
            AnnotationKind::Note => {
                let has_text = self
                    .note
                    .as_ref()
                    .is_some_and(|text| !text.trim().is_empty());
                if !has_text {
                    return Err(ModelError::InvalidAnnotationShape {
                        kind: self.kind.as_str(),
                        detail: "笔记必须有正文",
                    });
                }
            }
        }

        Ok(())
    }
}

/// 判断范围是否可能倒置。
///
/// 只在**能确定**时返回 `true`：优先用 PDF 页号、TXT 字符偏移这类整数锚点比较，
/// 再退回 `progression`。CFI 的语义级排序由渲染层负责，模型层不猜。
fn is_reversed(start: &Locator, end: &Locator) -> bool {
    if let (Some(s), Some(e)) = (start.perch.pdf_page, end.perch.pdf_page) {
        return e < s;
    }
    if let (Some(s), Some(e)) = (start.perch.txt_offset, end.perch.txt_offset) {
        return e < s;
    }
    match (start.locations.progression, end.locations.progression) {
        (Some(s), Some(e)) => e < s,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn epub_at(href: &str, cfi: &str, progression: f64) -> Locator {
        Locator::new_epub(href, Some(cfi.to_owned()), Some(progression))
    }

    #[test]
    fn highlight_roundtrip() {
        let book_id = BookId::new();
        let ann = Annotation::new_highlight(
            book_id,
            epub_at("/c1.xhtml", "/6/4!/4/2:1", 0.10),
            epub_at("/c1.xhtml", "/6/4!/4/2:20", 0.12),
            Some("#FFD400".to_owned()),
            Some("被高亮的原文".to_owned()),
        );
        ann.validate().unwrap();
        assert!(ann.is_range());
        assert_eq!(ann.kind, AnnotationKind::Highlight);

        let json = serde_json::to_string(&ann).unwrap();
        assert!(json.contains("\"bookId\""), "json = {json}");
        assert!(json.contains("\"createdAt\""), "json = {json}");
        assert!(json.contains("\"kind\":\"highlight\""), "json = {json}");
        let back: Annotation = serde_json::from_str(&json).unwrap();
        assert_eq!(ann, back);
    }

    #[test]
    fn bookmark_is_point_anchored() {
        let ann = Annotation::new_bookmark(BookId::new(), epub_at("/c1.xhtml", "/6/4", 0.5));
        ann.validate().unwrap();
        assert!(!ann.is_range());
        assert!(ann.note.is_none());
    }

    #[test]
    fn bookmark_with_end_is_rejected() {
        let mut ann = Annotation::new_bookmark(BookId::new(), epub_at("/c1.xhtml", "/6/4", 0.5));
        ann.end = Some(epub_at("/c1.xhtml", "/6/6", 0.6));
        assert_eq!(
            ann.validate().unwrap_err().code(),
            "MODEL_INVALID_ANNOTATION_SHAPE"
        );
    }

    #[test]
    fn range_without_end_is_rejected() {
        let mut ann = Annotation::new_highlight(
            BookId::new(),
            epub_at("/c1.xhtml", "/6/4", 0.1),
            epub_at("/c1.xhtml", "/6/6", 0.2),
            None,
            None,
        );
        ann.end = None;
        assert_eq!(
            ann.validate().unwrap_err().code(),
            "MODEL_INVALID_ANNOTATION_SHAPE"
        );
    }

    #[test]
    fn range_must_stay_within_one_resource() {
        let ann = Annotation::new_highlight(
            BookId::new(),
            epub_at("/c1.xhtml", "/6/4", 0.1),
            epub_at("/c2.xhtml", "/6/4", 0.2),
            None,
            None,
        );
        assert_eq!(
            ann.validate().unwrap_err().code(),
            "MODEL_INVALID_ANNOTATION_SHAPE"
        );
    }

    #[test]
    fn reversed_range_is_rejected() {
        // 用文字偏移构造一个明确倒置的范围
        let start = Locator::new_txt(900, None, Some(1_000));
        let mut end = Locator::new_txt(100, None, Some(1_000));
        let ann = Annotation::new_highlight(BookId::new(), start, end.clone(), None, None);
        assert_eq!(ann.validate().unwrap_err().code(), "MODEL_INVALID_RANGE");

        // 反方向是合法的
        end.perch.txt_offset = Some(950);
        let ok = Annotation::new_highlight(
            BookId::new(),
            Locator::new_txt(900, None, Some(1_000)),
            end,
            None,
            None,
        );
        ok.validate().unwrap();
    }

    #[test]
    fn reversed_pdf_pages_are_rejected() {
        let ann = Annotation::new_highlight(
            BookId::new(),
            Locator::new_pdf(10, Some(100)),
            Locator::new_pdf(3, Some(100)),
            None,
            None,
        );
        assert_eq!(ann.validate().unwrap_err().code(), "MODEL_INVALID_RANGE");
    }

    #[test]
    fn note_requires_text() {
        let mut ann =
            Annotation::new_note(BookId::new(), epub_at("/c1.xhtml", "/6/4", 0.5), "想法");
        ann.validate().unwrap();

        ann.note = Some("   ".to_owned());
        assert_eq!(
            ann.validate().unwrap_err().code(),
            "MODEL_INVALID_ANNOTATION_SHAPE"
        );

        ann.note = None;
        assert_eq!(
            ann.validate().unwrap_err().code(),
            "MODEL_INVALID_ANNOTATION_SHAPE"
        );
    }

    #[test]
    fn invalid_inner_locator_propagates() {
        let mut ann = Annotation::new_bookmark(BookId::new(), epub_at("/c1.xhtml", "/6/4", 0.5));
        ann.start.href = String::new();
        assert_eq!(ann.validate().unwrap_err().code(), "MODEL_EMPTY_FIELD");
    }

    #[test]
    fn tombstone_is_serialized() {
        let mut ann = Annotation::new_bookmark(BookId::new(), epub_at("/c1.xhtml", "/6/4", 0.5));
        ann.deleted = true;
        let json = serde_json::to_string(&ann).unwrap();
        assert!(json.contains("\"deleted\":true"), "json = {json}");
    }

    #[test]
    fn annotation_id_parses_and_rejects() {
        let id = AnnotationId::new();
        assert_eq!(AnnotationId::parse(&id.to_string()).unwrap(), id);
        assert_eq!(
            AnnotationId::parse("nope").unwrap_err().code(),
            "MODEL_INVALID_ID"
        );
    }
}
