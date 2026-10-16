//! 书籍元数据与**导入状态机**。
//!
//! 导入被拆成三个阶段（见 `docs/02-架构设计.md` §六），状态机把"哪个阶段该做什么"
//! 编码成可测试的约束——这比散落在各处的 `if state == ...` 可靠得多：
//!
//! ```text
//!               指纹去重通过
//!   registered ──────────────► refining ──► indexing ──► ready（终态）
//!        │                        │             │
//!        └──────────► failed ◄────┴─────────────┘
//!                      │
//!                      └── 重试 ──► registered
//! ```
//!
//! 每个阶段都必须**幂等**：崩溃后从指纹恢复，任一步都可以安全重跑。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ModelError;
use crate::locator::Format;
use crate::timestamp::Timestamp;
use crate::util::is_false;

/// 书籍唯一标识（UUID v4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BookId(Uuid);

impl BookId {
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
                kind: "book",
                value: value.to_owned(),
            })
    }
}

impl Default for BookId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for BookId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 导入状态：三阶段管线 + 终态 + 失败态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportState {
    /// 阶段①：文件已落盘、指纹与记录已写入。**尚未解析任何内容**。
    Registered,
    /// 阶段②：后台抽取元数据 / 封面 / 目录 / 字数。
    Refining,
    /// 阶段③：后台建立全文索引。
    Indexing,
    /// 可用（终态）。
    Ready,
    /// 任一阶段失败（可重试）。
    Failed,
}

impl ImportState {
    /// 稳定的字符串表示。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Refining => "refining",
            Self::Indexing => "indexing",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    /// 是否为终态（不再需要后台工作）。
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Ready)
    }

    /// 该状态下是否仍有后台工作待做（用于调度与进度提示）。
    #[must_use]
    pub const fn needs_background_work(self) -> bool {
        matches!(self, Self::Registered | Self::Refining | Self::Indexing)
    }

    /// 是否允许从当前状态迁移到 `next`。
    ///
    /// 刻意不允许 `ready → *`：书一旦可用，重新解析应由显式的"重新精修"入口触发，
    /// 而不是被隐式状态流转带回去。
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Registered, Self::Refining)
                | (Self::Registered, Self::Failed)
                | (Self::Refining, Self::Indexing)
                | (Self::Refining, Self::Ready)
                | (Self::Refining, Self::Failed)
                | (Self::Indexing, Self::Ready)
                | (Self::Indexing, Self::Failed)
                | (Self::Failed, Self::Registered)
        )
    }
}

impl std::fmt::Display for ImportState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一本书的元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Book {
    /// 唯一标识。
    pub id: BookId,
    /// 标题（不能为空）。
    pub title: String,
    /// 作者。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// 格式。
    pub format: Format,
    /// 出版社。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// 语言（BCP-47，如 `zh-Hans`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// 文件字节数。
    pub size_bytes: u64,
    /// 文件指纹（十六进制）。**导入幂等键**：指纹相同即视为同一本书。
    pub fingerprint: String,
    /// 导入状态。
    pub state: ImportState,
    /// 封面图路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_path: Option<String>,
    /// 字数（精修阶段统计）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub word_count: Option<u64>,
    /// 加入书库的时间。
    pub added_at: Timestamp,
    /// 最近打开时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_opened_at: Option<Timestamp>,
    /// 软删除（墓碑）。同步完成前不物理删除。
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

impl Book {
    /// 登记一本书（阶段①的产物）。此时还不知道元数据，先用文件名兜底当标题。
    #[must_use]
    pub fn register(
        title: impl Into<String>,
        format: Format,
        size_bytes: u64,
        fingerprint: impl Into<String>,
    ) -> Self {
        Self {
            id: BookId::new(),
            title: title.into(),
            author: None,
            format,
            publisher: None,
            language: None,
            size_bytes,
            fingerprint: fingerprint.into(),
            state: ImportState::Registered,
            cover_path: None,
            word_count: None,
            added_at: Timestamp::now(),
            last_opened_at: None,
            deleted: false,
        }
    }

    /// 推进导入状态。
    ///
    /// # Errors
    ///
    /// 非法迁移返回 [`ModelError::InvalidTransition`]。
    pub fn advance_to(&mut self, next: ImportState) -> Result<(), ModelError> {
        let current = self.state;
        if !current.can_transition_to(next) {
            return Err(ModelError::InvalidTransition {
                from: current.as_str(),
                to: next.as_str(),
            });
        }
        self.state = next;
        Ok(())
    }

    /// 校验不变量。
    ///
    /// # Errors
    ///
    /// - 标题为空 → [`ModelError::EmptyField`]
    /// - 指纹为空 → [`ModelError::EmptyField`]
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.title.trim().is_empty() {
            return Err(ModelError::EmptyField { field: "title" });
        }
        if self.fingerprint.trim().is_empty() {
            return Err(ModelError::EmptyField {
                field: "fingerprint",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn happy_path_transitions() {
        let mut book = Book::register("测试书", Format::Epub, 1024, "abc123");
        assert_eq!(book.state, ImportState::Registered);
        assert!(book.state.needs_background_work());
        assert!(!book.state.is_terminal());

        book.advance_to(ImportState::Refining).unwrap();
        book.advance_to(ImportState::Indexing).unwrap();
        book.advance_to(ImportState::Ready).unwrap();
        assert!(book.state.is_terminal());
        assert!(!book.state.needs_background_work());
    }

    #[test]
    fn refining_may_skip_indexing() {
        // 纯 TXT 的书可能不需要建索引，直接从精修到可用
        let mut book = Book::register("t", Format::Txt, 10, "f");
        book.advance_to(ImportState::Refining).unwrap();
        book.advance_to(ImportState::Ready).unwrap();
        assert_eq!(book.state, ImportState::Ready);
    }

    #[test]
    fn failed_can_be_retried() {
        let mut book = Book::register("t", Format::Epub, 10, "f");
        book.advance_to(ImportState::Failed).unwrap();
        book.advance_to(ImportState::Registered).unwrap();
        assert_eq!(book.state, ImportState::Registered);
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        // 不允许跳过精修直接索引
        let mut book = Book::register("t", Format::Epub, 10, "f");
        let err = book.advance_to(ImportState::Indexing).unwrap_err();
        assert_eq!(err.code(), "MODEL_INVALID_TRANSITION");
        // 状态没有被改动
        assert_eq!(book.state, ImportState::Registered);

        // ready 是终态，不能再流转
        let mut ready = Book::register("t", Format::Epub, 10, "f");
        ready.advance_to(ImportState::Refining).unwrap();
        ready.advance_to(ImportState::Ready).unwrap();
        assert_eq!(
            ready.advance_to(ImportState::Indexing).unwrap_err().code(),
            "MODEL_INVALID_TRANSITION"
        );
        // 自环也不允许
        assert!(!ImportState::Registered.can_transition_to(ImportState::Registered));
    }

    #[test]
    fn every_state_has_a_stable_name() {
        for state in [
            ImportState::Registered,
            ImportState::Refining,
            ImportState::Indexing,
            ImportState::Ready,
            ImportState::Failed,
        ] {
            assert_eq!(state.to_string(), state.as_str());
            assert!(!state.as_str().is_empty());
        }
    }

    #[test]
    fn book_id_parses_and_rejects() {
        let id = BookId::new();
        let text = id.to_string();
        assert_eq!(BookId::parse(&text).unwrap(), id);

        let err = BookId::parse("not-a-uuid").unwrap_err();
        assert_eq!(err.code(), "MODEL_INVALID_ID");
    }

    #[test]
    fn book_validation_rejects_blank_fields() {
        let mut book = Book::register("书名", Format::Epub, 1, "fp");
        book.validate().unwrap();

        book.title = "  ".to_owned();
        assert_eq!(book.validate().unwrap_err().code(), "MODEL_EMPTY_FIELD");

        book.title = "书名".to_owned();
        book.fingerprint = String::new();
        assert_eq!(book.validate().unwrap_err().code(), "MODEL_EMPTY_FIELD");
    }

    #[test]
    fn book_json_uses_camel_case_and_skips_empty_optionals() {
        let book = Book::register("三体", Format::Epub, 2_048, "deadbeef");
        let json = serde_json::to_string(&book).unwrap();
        assert!(json.contains("\"sizeBytes\""), "json = {json}");
        assert!(json.contains("\"addedAt\""), "json = {json}");
        assert!(json.contains("\"state\":\"registered\""), "json = {json}");
        // 可选项与 false 墓碑不应出现在 JSON 里
        assert!(!json.contains("author"), "json = {json}");
        assert!(!json.contains("deleted"), "json = {json}");

        let back: Book = serde_json::from_str(&json).unwrap();
        assert_eq!(book, back);
    }

    #[test]
    fn deleted_book_is_serialized_as_tombstone() {
        let mut book = Book::register("t", Format::Pdf, 1, "f");
        book.deleted = true;
        let json = serde_json::to_string(&book).unwrap();
        assert!(json.contains("\"deleted\":true"), "json = {json}");
    }
}
