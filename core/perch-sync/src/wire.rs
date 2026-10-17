//! 同步的**传输层**：变更的线上表示、增量游标、变更日志。
//!
//! `lib.rs` 里的 [`Lww`](crate::Lww) / [`merge_progress`](crate::merge_progress) 是
//! 「合并算子」，本模块把它们接到「一条一条变更如何在两端之间搬运」上。
//!
//! # 为什么是状态型同步，而不是操作日志
//!
//! 每个 `(书, 标注)` 组合是一个 **LWW 寄存器**：日志里只保留该寄存器的**当前胜者**，
//! 不保留历史。理由：
//!
//! - 阅读器的同步对象是"当前进度""当前标注集合"，不是"编辑历史"；
//! - 只保胜者 ⇒ 数据量与设备离线时长无关，一个离线三个月的设备重连也只传当前值；
//! - 冲突率的口径（见 `docs/02`）本来就是"被丢弃的字段级变更 / 总字段级变更"，
//!   与"只保胜者"一致。
//!
//! 代价：**没有撤销历史**，也不能做"挑一个旧版本"这种功能。这是有意的取舍。
//!
//! # 变更是什么
//!
//! 一条 [`Change`] = 「对某个目标（一本书的进度 / 一条标注）在某个 HLC 上的设置或删除」。
//!
//! ```text
//! { "book": "…", "annotation": null, "hlc": "1700000000000-0-iphone",
//!   "op": "set", "value": { "kind": "progress", "locator": { … } } }
//! { "book": "…", "annotation": "…", "hlc": "1700000000001-0-ipad", "op": "tombstone" }
//! ```
//!
//! # 增量游标
//!
//! 客户端带上 `since`（上次同步拿到的最大的 HLC），服务端返回
//! **HLC 严格大于它**的寄存器。因为 HLC 是全序且唯一（wall, counter, device），
//! 这个游标既不会漏也不会重。分页用 [`DEFAULT_PAGE_LIMIT`]，`has_more` 表示还有。
//!
//! # 编码
//!
//! v1 用 JSON（[`SyncRequest::to_json`] / [`SyncRequest::from_json`]）。
//! 传输层只认字节序列，协议是 HTTP / WebSocket / 文件同步都不影响本模块。
//! 将来要做二进制编码，只需换掉这两个函数，数据结构不用动。

use std::collections::BTreeMap;

use perch_model::{Annotation, AnnotationId, BookId, Locator};
use serde::{Deserialize, Serialize};

use crate::{Hlc, Lww, MergeEffect, MergeStats};

/// 当前协议版本。
///
/// 版本只在**不兼容**时才自增：新增可选字段不算。接收方接受
/// `收到版本 <= 本端版本`（旧客户端可以连新服务端），
/// 但拒绝更高版本，因为不知道该怎么降级。
pub const PROTOCOL_VERSION: u16 = 1;

/// 增量同步默认分页大小。
///
/// 取 500 是折中：一本书的进度 + 几千条标注在几十页内能传完，
/// 单页又足够小，不会让一次请求的内存和延迟失控。
pub const DEFAULT_PAGE_LIMIT: usize = 500;

/// 同步传输层错误。
///
/// 每个变体都有稳定的 [`SyncError::code`]，供 FFI 映射成错误码字符串
/// （约定与 `perch-model` / `perch-index` 一致）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyncError {
    /// 协议版本过高，本端无法解析。
    #[error("协议版本不支持：收到 {got}，本端最高支持 {max}")]
    UnsupportedProtocol {
        /// 收到的版本。
        got: u16,
        /// 本端支持的最高版本。
        max: u16,
    },

    /// 变更的目标与值类型不匹配（例如给标注目标塞了一个进度值）。
    #[error("变更目标与值类型不匹配：{detail}")]
    TargetMismatch {
        /// 说明。
        detail: &'static str,
    },

    /// 变更内部自相矛盾（标注 id / 所属书 / 自带 HLC 与目标不一致）。
    #[error("变更内部不一致：{detail}")]
    Inconsistent {
        /// 说明。
        detail: &'static str,
    },

    /// 序列化失败。
    #[error("编码失败：{reason}")]
    Encode {
        /// 原因。
        reason: String,
    },

    /// 反序列化失败。
    #[error("解码失败：{reason}")]
    Decode {
        /// 原因。
        reason: String,
    },
}

impl SyncError {
    /// 稳定的错误码字符串。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedProtocol { .. } => "SYNC_UNSUPPORTED_PROTOCOL",
            Self::TargetMismatch { .. } => "SYNC_TARGET_MISMATCH",
            Self::Inconsistent { .. } => "SYNC_INCONSISTENT",
            Self::Encode { .. } => "SYNC_ENCODE",
            Self::Decode { .. } => "SYNC_DECODE",
        }
    }
}

/// 检查对端协议版本是否可接受。
///
/// # Errors
///
/// 版本为 0（无效）或高于 [`PROTOCOL_VERSION`] 时返回
/// [`SyncError::UnsupportedProtocol`]。
pub fn check_protocol(got: u16) -> Result<(), SyncError> {
    if got == 0 || got > PROTOCOL_VERSION {
        return Err(SyncError::UnsupportedProtocol {
            got,
            max: PROTOCOL_VERSION,
        });
    }
    Ok(())
}

/// 变更携带的值。
///
/// 只有两种：**阅读进度**（一本书一个寄存器）和**整条标注**
/// （一条标注一个寄存器，范围标注的两端整体搬运，不会撕裂）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum WireValue {
    /// 阅读进度。
    Progress {
        /// 位置。
        ///
        /// 装箱的原因：`Annotation` 接近 1KB，不装箱的话 [`Change`] 单条也要 1KB，
        /// 一页 500 条就是 500KB。装箱后 `Change` 只有几十字节。
        /// JSON 表示不受影响（`Box<T>` 与 `T` 序列化结果相同）。
        locator: Box<Locator>,
    },
    /// 整条标注。
    Annotation {
        /// 标注本体。
        annotation: Box<Annotation>,
    },
}

impl WireValue {
    /// 构造一个进度值。
    #[must_use]
    pub fn progress(locator: Locator) -> Self {
        Self::Progress {
            locator: Box::new(locator),
        }
    }

    /// 构造一个标注值。
    #[must_use]
    pub fn annotation(annotation: Annotation) -> Self {
        Self::Annotation {
            annotation: Box::new(annotation),
        }
    }

    /// 若这是进度值，取出位置。
    #[must_use]
    pub fn as_progress(&self) -> Option<&Locator> {
        match self {
            Self::Progress { locator } => Some(locator),
            Self::Annotation { .. } => None,
        }
    }

    /// 若这是标注值，取出标注。
    #[must_use]
    pub fn as_annotation(&self) -> Option<&Annotation> {
        match self {
            Self::Annotation { annotation } => Some(annotation),
            Self::Progress { .. } => None,
        }
    }
}

/// 变更操作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "op")]
pub enum ChangeOp {
    /// 写入一个值。
    Set {
        /// 新值。
        value: WireValue,
    },
    /// 删除（墓碑）。只带时间戳，不带值 —— 墓碑要能比正文活得久。
    Tombstone,
}

impl ChangeOp {
    /// 写入一个值。
    #[must_use]
    pub const fn set(value: WireValue) -> Self {
        Self::Set { value }
    }

    /// 删除（墓碑）。
    #[must_use]
    pub const fn tombstone() -> Self {
        Self::Tombstone
    }

    /// 是否为墓碑。
    #[must_use]
    pub const fn is_tombstone(&self) -> bool {
        matches!(self, Self::Tombstone)
    }
}

/// 一条变更。
///
/// `annotation` 为 `None` 表示目标是**这本书的进度**；为 `Some(id)` 表示目标是**该条标注**。
/// 这一个字段就决定了寄存器身份，因此 [`Change::key`] 是全序的。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    /// 所属书。
    pub book: BookId,
    /// 标注 id；`None` 表示书的阅读进度。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<AnnotationId>,
    /// 排序时间戳（HLC）。**这是线上唯一的排序依据。**
    pub hlc: Hlc,
    /// 操作（写入或删除）。
    #[serde(flatten)]
    pub op: ChangeOp,
}

impl Change {
    /// 该变更作用的寄存器。
    #[must_use]
    pub const fn key(&self) -> ChangeKey {
        ChangeKey {
            book: self.book,
            annotation: self.annotation,
        }
    }

    /// 校验内部一致性。
    ///
    /// # Errors
    ///
    /// - [`SyncError::TargetMismatch`]：进度值配了标注 id，或标注值没配标注 id
    /// - [`SyncError::Inconsistent`]：标注 id / 所属书 / 自带 HLC 与变更目标不符
    pub fn validate(&self) -> Result<(), SyncError> {
        // 墓碑对两种目标都合法，不需要进一步检查
        let ChangeOp::Set { value } = &self.op else {
            return Ok(());
        };

        match value {
            WireValue::Progress { .. } => {
                if self.annotation.is_some() {
                    return Err(SyncError::TargetMismatch {
                        detail: "进度变更不能带标注 id",
                    });
                }
            }
            WireValue::Annotation { annotation } => {
                let Some(id) = self.annotation else {
                    return Err(SyncError::TargetMismatch {
                        detail: "标注变更必须带标注 id",
                    });
                };
                if annotation.id != id {
                    return Err(SyncError::Inconsistent {
                        detail: "标注自带 id 与变更目标不一致",
                    });
                }
                if annotation.book_id != self.book {
                    return Err(SyncError::Inconsistent {
                        detail: "标注所属书与变更目标不一致",
                    });
                }
                // 标注自身也带 hlc 字段（本地存储用）。若两端都填了，必须一致，
                // 否则就出现了「两个时间戳」这种迟早出事的状态。
                if let Some(text) = annotation.hlc.as_deref() {
                    match Hlc::parse(text) {
                        Some(stamp) if stamp == self.hlc => {}
                        _ => {
                            return Err(SyncError::Inconsistent {
                                detail: "标注自带 HLC 与变更 HLC 不一致",
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// 该变更是"删除"吗？
    ///
    /// 墓碑算删除；本地存储把 `Annotation.deleted` 置位的也算删除
    /// （两种表示在 `perch-model` 里都存在，这里统一口径）。
    #[must_use]
    pub fn is_deletion(&self) -> bool {
        match &self.op {
            ChangeOp::Tombstone => true,
            ChangeOp::Set { value } => match value.as_annotation() {
                Some(annotation) => annotation.deleted,
                None => false,
            },
        }
    }
}

/// 寄存器的身份：一本书的进度，或一本书里的某条标注。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChangeKey {
    /// 所属书。
    pub book: BookId,
    /// 标注 id；`None` 表示阅读进度。
    pub annotation: Option<AnnotationId>,
}

/// 一次性校验一批变更。
///
/// 批量入账前先整体校验，避免"改了一半才发现第 37 条是坏的"。
///
/// # Errors
///
/// 任意一条不合法时返回该条的错误。
pub fn validate_all(changes: &[Change]) -> Result<(), SyncError> {
    for change in changes {
        change.validate()?;
    }
    Ok(())
}

/// 一轮合并的明细与统计。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MergeReport {
    /// 每条变更的合并效果，顺序与输入一致。
    pub effects: Vec<MergeEffect>,
    /// 计数统计（冲突率就是从这里算的）。
    pub stats: MergeStats,
}

impl MergeReport {
    /// 记录一次效果。
    pub fn push(&mut self, effect: MergeEffect) {
        self.effects.push(effect);
        self.stats.record(effect);
    }

    /// 冲突率（口径见 `docs/02`，实现见 [`MergeStats::conflict_rate`]）。
    #[must_use]
    pub fn conflict_rate(&self) -> f32 {
        self.stats.conflict_rate()
    }
}

/// 一页增量结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ChangePage {
    /// 本页变更。
    pub changes: Vec<Change>,
    /// 下次请求应当带上的游标。
    pub next_cursor: Option<Hlc>,
    /// 是否还有下一页。
    pub has_more: bool,
}

/// 变更日志：**每个寄存器保留当前胜者**的 LWW 表。
///
/// 客户端和服务端用的是同一套类型——服务端只是"所有人的日志的并集"。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChangeLog {
    entries: BTreeMap<ChangeKey, Lww<ChangeOp>>,
}

impl ChangeLog {
    /// 空日志。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 寄存器数量。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 应用一条变更，返回合并效果。
    ///
    /// # Errors
    ///
    /// 变更不合法时返回 [`SyncError`]，且**不会**写入。
    pub fn apply(&mut self, change: Change) -> Result<MergeEffect, SyncError> {
        change.validate()?;
        let key = change.key();
        let incoming = Lww::new(change.op, change.hlc);
        match self.entries.get_mut(&key) {
            Some(existing) => Ok(existing.merge(incoming)),
            None => {
                self.entries.insert(key, incoming);
                Ok(MergeEffect::Initialized)
            }
        }
    }

    /// 批量应用。
    ///
    /// ⚠️ 逐条应用：若第 N 条不合法，**前 N-1 条已经写入**。
    /// 需要全有或全无时，先用 [`validate_all`] 校验。
    ///
    /// # Errors
    ///
    /// 任意一条不合法时返回该条的错误。
    pub fn apply_all(&mut self, changes: &[Change]) -> Result<MergeReport, SyncError> {
        let mut report = MergeReport::default();
        for change in changes {
            let effect = self.apply(change.clone())?;
            report.push(effect);
        }
        Ok(report)
    }

    /// 取 `since` 之后的寄存器，按 HLC 升序分页（增量同步的服务端实现）。
    ///
    /// `since` 为 `None` 表示"从头开始"（首次同步）。
    #[must_use]
    pub fn changes_since(&self, since: Option<&Hlc>, limit: usize) -> ChangePage {
        let mut matched: Vec<(&ChangeKey, &Lww<ChangeOp>)> = self
            .entries
            .iter()
            .filter(|(_, entry)| match since {
                None => true,
                Some(cursor) => entry.stamp > *cursor,
            })
            .collect();
        // HLC 是全序且唯一的（wall, counter, device），因此这里的排序是确定的
        matched.sort_by(|left, right| left.1.stamp.cmp(&right.1.stamp));

        let page_size = limit.max(1);
        let has_more = matched.len() > page_size;
        let page = &matched[..matched.len().min(page_size)];

        let changes: Vec<Change> = page
            .iter()
            .map(|(key, entry)| Change {
                book: key.book,
                annotation: key.annotation,
                hlc: entry.stamp.clone(),
                op: entry.value.clone(),
            })
            .collect();

        let next_cursor = changes
            .last()
            .map(|change| change.hlc.clone())
            .or_else(|| since.cloned());

        ChangePage {
            changes,
            next_cursor,
            has_more,
        }
    }

    /// 导出全部寄存器（用于"把本地状态整体推给服务端"或落盘）。
    #[must_use]
    pub fn to_changes(&self) -> Vec<Change> {
        self.entries
            .iter()
            .map(|(key, entry)| Change {
                book: key.book,
                annotation: key.annotation,
                hlc: entry.stamp.clone(),
                op: entry.value.clone(),
            })
            .collect()
    }

    /// 由一组变更重建日志。
    ///
    /// # Errors
    ///
    /// 任意一条不合法时返回该条的错误（此时返回值不可用）。
    pub fn from_changes(changes: &[Change]) -> Result<Self, SyncError> {
        validate_all(changes)?;
        let mut log = Self::new();
        log.apply_all(changes)?;
        Ok(log)
    }

    /// 取某本书的当前阅读进度（墓碑或从未记录时返回 `None`）。
    #[must_use]
    pub fn progress_of(&self, book: BookId) -> Option<&Locator> {
        let key = ChangeKey {
            book,
            annotation: None,
        };
        let entry = self.entries.get(&key)?;
        match &entry.value {
            ChangeOp::Set { value } => value.as_progress(),
            ChangeOp::Tombstone => None,
        }
    }

    /// 取某条标注（墓碑或不存在时返回 `None`）。
    #[must_use]
    pub fn annotation(&self, id: AnnotationId) -> Option<&Annotation> {
        self.entries
            .iter()
            .find(|(key, _)| key.annotation == Some(id))
            .and_then(|(_, entry)| match &entry.value {
                ChangeOp::Set { value } => value
                    .as_annotation()
                    .filter(|annotation| !annotation.deleted),
                ChangeOp::Tombstone => None,
            })
    }

    /// 某本书的全部有效标注（不含墓碑）。
    #[must_use]
    pub fn annotations_of(&self, book: BookId) -> Vec<&Annotation> {
        self.entries
            .iter()
            .filter(|(key, _)| key.book == book && key.annotation.is_some())
            .filter_map(|(_, entry)| match &entry.value {
                ChangeOp::Set { value } => value
                    .as_annotation()
                    .filter(|annotation| !annotation.deleted),
                ChangeOp::Tombstone => None,
            })
            .collect()
    }

    /// 取某个寄存器的原始记录（含墓碑），调试与测试用。
    #[must_use]
    pub fn entry(&self, key: &ChangeKey) -> Option<&Lww<ChangeOp>> {
        self.entries.get(key)
    }
}

fn default_limit() -> usize {
    DEFAULT_PAGE_LIMIT
}

/// 客户端发起的同步请求。
///
/// 一次请求同时完成「推」和「拉」：把本地变更推上去，顺便取回 `since` 之后的增量。
/// 一次往返完成双向同步，弱网下比 push/pull 两趟更省。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncRequest {
    /// 协议版本。
    pub protocol: u16,
    /// 发起方设备号（HLC 里的 device 必须与之一致）。
    pub device: String,
    /// 上次同步拿到的游标；`None` 表示首次同步。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Hlc>,
    /// 本页最多要多少条。
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// 本地待上传的变更。
    #[serde(default)]
    pub changes: Vec<Change>,
}

impl SyncRequest {
    /// 构造一个空的拉取请求。
    #[must_use]
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            device: device.into(),
            since: None,
            limit: DEFAULT_PAGE_LIMIT,
            changes: Vec::new(),
        }
    }

    /// 校验协议版本与变更合法性。
    ///
    /// # Errors
    ///
    /// 版本不支持或任一变更不合法时返回错误。
    pub fn validate(&self) -> Result<(), SyncError> {
        check_protocol(self.protocol)?;
        validate_all(&self.changes)
    }

    /// 序列化为 JSON 文本。
    ///
    /// # Errors
    ///
    /// 序列化失败时返回 [`SyncError::Encode`]。
    pub fn to_json(&self) -> Result<String, SyncError> {
        serde_json::to_string(self).map_err(|error| SyncError::Encode {
            reason: error.to_string(),
        })
    }

    /// 从 JSON 文本解析并校验版本。
    ///
    /// # Errors
    ///
    /// 解析失败返回 [`SyncError::Decode`]；版本不支持返回
    /// [`SyncError::UnsupportedProtocol`]。
    pub fn from_json(text: &str) -> Result<Self, SyncError> {
        let request: Self = serde_json::from_str(text).map_err(|error| SyncError::Decode {
            reason: error.to_string(),
        })?;
        check_protocol(request.protocol)?;
        Ok(request)
    }
}

/// 服务端的同步响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResponse {
    /// 协议版本。
    pub protocol: u16,
    /// `since` 之后的增量。
    #[serde(default)]
    pub changes: Vec<Change>,
    /// 下次请求应当带上的游标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Hlc>,
    /// 是否还有下一页。
    #[serde(default)]
    pub has_more: bool,
    /// 服务端对本次上传变更的处理统计（冲突率从这里算）。
    #[serde(default)]
    pub stats: MergeStats,
}

impl SyncResponse {
    /// 由一页增量与一个合并报告构造响应。
    #[must_use]
    pub fn from_page(page: ChangePage, stats: MergeStats) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            changes: page.changes,
            next_cursor: page.next_cursor,
            has_more: page.has_more,
            stats,
        }
    }

    /// 序列化为 JSON 文本。
    ///
    /// # Errors
    ///
    /// 序列化失败时返回 [`SyncError::Encode`]。
    pub fn to_json(&self) -> Result<String, SyncError> {
        serde_json::to_string(self).map_err(|error| SyncError::Encode {
            reason: error.to_string(),
        })
    }

    /// 从 JSON 文本解析并校验版本。
    ///
    /// # Errors
    ///
    /// 解析失败返回 [`SyncError::Decode`]；版本不支持返回
    /// [`SyncError::UnsupportedProtocol`]。
    pub fn from_json(text: &str) -> Result<Self, SyncError> {
        let response: Self = serde_json::from_str(text).map_err(|error| SyncError::Decode {
            reason: error.to_string(),
        })?;
        check_protocol(response.protocol)?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use perch_model::Annotation;

    fn stamp(wall: i64, counter: u32, device: &str) -> Hlc {
        Hlc::from_parts(wall, counter, device)
    }

    fn locator(progression: f64) -> Locator {
        Locator::new_epub("/c1.xhtml", Some("/6/4".to_owned()), Some(progression))
    }

    fn progress_change(book: BookId, wall: i64, progression: f64) -> Change {
        Change {
            book,
            annotation: None,
            hlc: stamp(wall, 0, "iphone"),
            op: ChangeOp::set(WireValue::progress(locator(progression))),
        }
    }

    fn annotation_change(book: BookId, wall: i64, note: &str) -> (Change, AnnotationId) {
        let mut annotation = Annotation::new_note(book, locator(0.25), note);
        annotation.hlc = Some(stamp(wall, 0, "iphone").to_string());
        let id = annotation.id;
        (
            Change {
                book,
                annotation: Some(id),
                hlc: stamp(wall, 0, "iphone"),
                op: ChangeOp::set(WireValue::annotation(annotation)),
            },
            id,
        )
    }

    fn tombstone(book: BookId, annotation: Option<AnnotationId>, wall: i64) -> Change {
        Change {
            book,
            annotation,
            hlc: stamp(wall, 0, "ipad"),
            op: ChangeOp::tombstone(),
        }
    }

    // ---------- 协议版本 ----------

    #[test]
    fn protocol_version_gate() {
        assert!(check_protocol(PROTOCOL_VERSION).is_ok());
        assert!(check_protocol(0).is_err());
        let error = check_protocol(PROTOCOL_VERSION + 1).unwrap_err();
        assert_eq!(error.code(), "SYNC_UNSUPPORTED_PROTOCOL");
        assert_eq!(
            error,
            SyncError::UnsupportedProtocol {
                got: PROTOCOL_VERSION + 1,
                max: PROTOCOL_VERSION,
            }
        );
    }

    // ---------- 校验 ----------

    #[test]
    fn progress_change_must_not_carry_annotation_id() {
        let book = BookId::new();
        let mut change = progress_change(book, 100, 0.5);
        change.annotation = Some(AnnotationId::new());
        assert_eq!(
            change.validate().unwrap_err().code(),
            "SYNC_TARGET_MISMATCH"
        );
    }

    #[test]
    fn annotation_change_must_carry_annotation_id() {
        let book = BookId::new();
        let (mut change, _) = annotation_change(book, 100, "笔记");
        change.annotation = None;
        assert_eq!(
            change.validate().unwrap_err().code(),
            "SYNC_TARGET_MISMATCH"
        );
    }

    #[test]
    fn annotation_id_must_match_target() {
        let book = BookId::new();
        let (mut change, _) = annotation_change(book, 100, "笔记");
        change.annotation = Some(AnnotationId::new());
        assert_eq!(change.validate().unwrap_err().code(), "SYNC_INCONSISTENT");
    }

    #[test]
    fn annotation_book_must_match_target() {
        let book = BookId::new();
        let (mut change, _) = annotation_change(book, 100, "笔记");
        change.book = BookId::new();
        // 注意：这里 annotation.book_id 与 change.book 不一致
        assert_eq!(change.validate().unwrap_err().code(), "SYNC_INCONSISTENT");
    }

    #[test]
    fn annotation_embedded_hlc_must_agree() {
        let book = BookId::new();
        let (mut change, _) = annotation_change(book, 100, "笔记");
        change.hlc = stamp(999, 0, "iphone");
        assert_eq!(change.validate().unwrap_err().code(), "SYNC_INCONSISTENT");
    }

    #[test]
    fn annotation_without_embedded_hlc_is_accepted() {
        let book = BookId::new();
        let mut annotation = Annotation::new_note(book, locator(0.1), "笔记");
        annotation.hlc = None;
        let change = Change {
            book,
            annotation: Some(annotation.id),
            hlc: stamp(100, 0, "iphone"),
            op: ChangeOp::set(WireValue::annotation(annotation)),
        };
        assert!(change.validate().is_ok());
    }

    #[test]
    fn tombstone_is_valid_for_both_targets() {
        let book = BookId::new();
        assert!(tombstone(book, None, 100).validate().is_ok());
        assert!(
            tombstone(book, Some(AnnotationId::new()), 100)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn validate_all_stops_at_first_bad_change() {
        let book = BookId::new();
        let good = progress_change(book, 100, 0.5);
        let mut bad = progress_change(book, 200, 0.6);
        bad.annotation = Some(AnnotationId::new());
        assert!(validate_all(std::slice::from_ref(&good)).is_ok());
        assert!(validate_all(&[good, bad]).is_err());
    }

    // ---------- 日志与合并 ----------

    #[test]
    fn first_write_initializes() {
        let book = BookId::new();
        let mut log = ChangeLog::new();
        assert_eq!(
            log.apply(progress_change(book, 100, 0.5)).unwrap(),
            MergeEffect::Initialized
        );
        assert_eq!(log.len(), 1);
        assert_eq!(
            log.progress_of(book).map(|l| l.locations.progression),
            Some(Some(0.5))
        );
    }

    #[test]
    fn newer_write_wins_and_older_is_ignored() {
        let book = BookId::new();
        let mut log = ChangeLog::new();
        log.apply(progress_change(book, 100, 0.5)).unwrap();
        assert_eq!(
            log.apply(progress_change(book, 200, 0.9)).unwrap(),
            MergeEffect::Updated
        );
        assert_eq!(
            log.apply(progress_change(book, 150, 0.1)).unwrap(),
            MergeEffect::StaleIgnored
        );
        assert_eq!(
            log.progress_of(book).map(|l| l.locations.progression),
            Some(Some(0.9))
        );
    }

    #[test]
    fn replaying_the_same_change_is_unchanged() {
        let book = BookId::new();
        let change = progress_change(book, 100, 0.5);
        let mut log = ChangeLog::new();
        log.apply(change.clone()).unwrap();
        assert_eq!(log.apply(change).unwrap(), MergeEffect::Unchanged);
        // 幂等：日志大小不变
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn invalid_change_is_not_written() {
        let book = BookId::new();
        let mut log = ChangeLog::new();
        let mut bad = progress_change(book, 100, 0.5);
        bad.annotation = Some(AnnotationId::new());
        assert!(log.apply(bad).is_err());
        assert!(log.is_empty(), "非法变更不能留下痕迹");
    }

    #[test]
    fn tombstone_wins_and_hides_values() {
        let book = BookId::new();
        let (change, id) = annotation_change(book, 100, "笔记");
        let mut log = ChangeLog::new();
        log.apply(change).unwrap();
        assert!(log.annotation(id).is_some());
        assert_eq!(log.annotations_of(book).len(), 1);

        assert_eq!(
            log.apply(tombstone(book, Some(id), 200)).unwrap(),
            MergeEffect::Updated
        );
        assert!(log.annotation(id).is_none(), "墓碑后不应再取到标注");
        assert!(log.annotations_of(book).is_empty());
    }

    #[test]
    fn deleting_progress_clears_it() {
        let book = BookId::new();
        let mut log = ChangeLog::new();
        log.apply(progress_change(book, 100, 0.5)).unwrap();
        log.apply(tombstone(book, None, 200)).unwrap();
        assert!(log.progress_of(book).is_none());
    }

    #[test]
    fn merge_report_counts_effects() {
        let book = BookId::new();
        let mut log = ChangeLog::new();
        let report = log
            .apply_all(&[
                progress_change(book, 100, 0.5),
                progress_change(book, 200, 0.6),
                progress_change(book, 150, 0.1),
            ])
            .unwrap();
        assert_eq!(report.effects.len(), 3);
        assert_eq!(report.stats.initialized, 1);
        assert_eq!(report.stats.updated, 1);
        assert_eq!(report.stats.stale_ignored, 1);
        assert_eq!(report.stats.conflicts, 0);
        assert!(report.conflict_rate() < 0.01, "不该有冲突");
    }

    #[test]
    fn unsupported_protocol_in_request() {
        let mut request = SyncRequest::new("iphone");
        request.protocol = 99;
        assert_eq!(
            request.validate().unwrap_err().code(),
            "SYNC_UNSUPPORTED_PROTOCOL"
        );
    }

    // ---------- 收敛性 ----------

    #[test]
    fn convergence_is_order_independent() {
        let book = BookId::new();
        let a = progress_change(book, 100, 0.1);
        let b = progress_change(book, 200, 0.2);
        let c = progress_change(book, 300, 0.3);

        let mut forward = ChangeLog::new();
        forward
            .apply_all(&[a.clone(), b.clone(), c.clone()])
            .unwrap();

        let mut backward = ChangeLog::new();
        backward
            .apply_all(&[c.clone(), b.clone(), a.clone()])
            .unwrap();

        let mut shuffled = ChangeLog::new();
        shuffled.apply_all(&[b, a, c]).unwrap();

        assert_eq!(forward, backward);
        assert_eq!(forward, shuffled);
        assert_eq!(
            forward.progress_of(book).map(|l| l.locations.progression),
            Some(Some(0.3))
        );
    }

    #[test]
    fn two_devices_converge_on_same_state() {
        let book = BookId::new();
        let phone_note = {
            let mut annotation = Annotation::new_note(book, locator(0.1), "手机写的");
            annotation.hlc = Some(stamp(100, 0, "iphone").to_string());
            Change {
                book,
                annotation: Some(annotation.id),
                hlc: stamp(100, 0, "iphone"),
                op: ChangeOp::set(WireValue::annotation(annotation)),
            }
        };
        let pad_progress = Change {
            book,
            annotation: None,
            hlc: stamp(110, 0, "ipad"),
            op: ChangeOp::set(WireValue::progress(locator(0.7))),
        };

        // 手机先本地记笔记，平板先本地记进度，然后互相交换
        let mut phone = ChangeLog::new();
        phone.apply(phone_note.clone()).unwrap();
        let mut pad = ChangeLog::new();
        pad.apply(pad_progress.clone()).unwrap();

        phone.apply_all(&pad.to_changes()).unwrap();
        pad.apply_all(&phone.to_changes()).unwrap();

        assert_eq!(phone, pad, "交换后两端必须收敛到同一状态");
    }

    // ---------- 增量游标 ----------

    #[test]
    fn incremental_cursor_returns_only_newer() {
        let book = BookId::new();
        let mut log = ChangeLog::new();
        log.apply(progress_change(book, 100, 0.1)).unwrap();
        log.apply(progress_change(book, 200, 0.2)).unwrap();

        let all = log.changes_since(None, DEFAULT_PAGE_LIMIT);
        assert_eq!(all.changes.len(), 1, "同一寄存器只保留胜者");
        assert!(!all.has_more);

        // 游标停在 200 之后 → 没有增量
        let cursor = all.next_cursor.clone();
        let none_new = log.changes_since(cursor.as_ref(), DEFAULT_PAGE_LIMIT);
        assert!(none_new.changes.is_empty());
        assert!(!none_new.has_more);

        // 游标早于 200 → 能拿到
        let older = log.changes_since(Some(&stamp(150, 0, "x")), DEFAULT_PAGE_LIMIT);
        assert_eq!(older.changes.len(), 1);
        assert_eq!(older.next_cursor, Some(stamp(200, 0, "iphone")));
    }

    #[test]
    fn paging_walks_the_whole_set_exactly_once() {
        let mut log = ChangeLog::new();
        let books: Vec<BookId> = (0..5).map(|_| BookId::new()).collect();
        for (i, book) in books.iter().enumerate() {
            log.apply(progress_change(*book, 100 + i as i64, 0.1 * i as f64))
                .unwrap();
        }

        let mut seen: Vec<BookId> = Vec::new();
        let mut cursor: Option<Hlc> = None;
        let mut rounds = 0;
        loop {
            rounds += 1;
            assert!(rounds < 20, "分页应当收敛");
            let page = log.changes_since(cursor.as_ref(), 2);
            seen.extend(page.changes.iter().map(|c| c.book));
            cursor = page.next_cursor;
            if !page.has_more {
                break;
            }
        }
        seen.sort();
        let mut expected = books;
        expected.sort();
        assert_eq!(seen, expected, "每本书恰好出现一次、不漏不重");
    }

    #[test]
    fn zero_limit_is_treated_as_one() {
        let book = BookId::new();
        let mut log = ChangeLog::new();
        log.apply(progress_change(book, 100, 0.5)).unwrap();
        let page = log.changes_since(None, 0);
        assert_eq!(page.changes.len(), 1);
    }

    // ---------- 线上格式 ----------

    #[test]
    fn request_round_trips_through_json() {
        let book = BookId::new();
        let mut request = SyncRequest::new("iphone");
        request.since = Some(stamp(100, 0, "ipad"));
        request.changes = vec![progress_change(book, 200, 0.42)];

        let text = request.to_json().unwrap();
        let back = SyncRequest::from_json(&text).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn response_round_trips_through_json() {
        let book = BookId::new();
        let mut log = ChangeLog::new();
        log.apply(progress_change(book, 100, 0.5)).unwrap();
        let response = SyncResponse::from_page(log.changes_since(None, 10), MergeStats::default());

        let text = response.to_json().unwrap();
        assert_eq!(SyncResponse::from_json(&text).unwrap(), response);
    }

    #[test]
    fn hlc_is_transmitted_as_text() {
        let book = BookId::new();
        let request = {
            let mut r = SyncRequest::new("iphone");
            r.changes = vec![progress_change(book, 100, 0.5)];
            r
        };
        let text = request.to_json().unwrap();
        // HLC 用可读文本，不用嵌套对象
        assert!(
            text.contains("\"hlc\":\"100-0-iphone\""),
            "hlc 应当是文本形式, json = {text}"
        );
    }

    #[test]
    fn json_shape_is_stable() {
        let book = BookId::new();
        let change = progress_change(book, 100, 0.5);
        let json = serde_json::to_value(&change).unwrap();
        // 关键字段名必须稳定：这是跨版本契约
        assert!(json.get("book").is_some());
        assert!(json.get("hlc").is_some());
        assert_eq!(json.get("op").and_then(|v| v.as_str()), Some("set"));
        assert_eq!(
            json.get("value")
                .and_then(|v| v.get("kind"))
                .and_then(|v| v.as_str()),
            Some("progress")
        );
        // 进度变更不该出现 annotation 字段
        assert!(json.get("annotation").is_none());
    }

    #[test]
    fn tombstone_json_has_no_value() {
        let book = BookId::new();
        let json = serde_json::to_value(tombstone(book, None, 100)).unwrap();
        assert_eq!(json.get("op").and_then(|v| v.as_str()), Some("tombstone"));
        assert!(json.get("value").is_none());
    }

    #[test]
    fn malformed_json_is_reported_not_panicking() {
        assert_eq!(
            SyncRequest::from_json("{").unwrap_err().code(),
            "SYNC_DECODE"
        );
        assert_eq!(
            SyncRequest::from_json(r#"{"protocol":1,"device":"a","changes":[{}]}"#)
                .unwrap_err()
                .code(),
            "SYNC_DECODE"
        );
        // HLC 文本坏了也不该 panic
        let bad = r#"{"protocol":1,"device":"a","changes":[{"book":"11111111-1111-4111-8111-111111111111","hlc":"不是HLC","op":"tombstone"}]}"#;
        assert_eq!(
            SyncRequest::from_json(bad).unwrap_err().code(),
            "SYNC_DECODE"
        );
    }

    #[test]
    fn request_from_json_rejects_future_protocol() {
        let text = r#"{"protocol":99,"device":"a","changes":[]}"#;
        assert_eq!(
            SyncRequest::from_json(text).unwrap_err().code(),
            "SYNC_UNSUPPORTED_PROTOCOL"
        );
    }

    #[test]
    fn defaults_are_applied_when_optional_fields_missing() {
        let text = r#"{"protocol":1,"device":"a"}"#;
        let request = SyncRequest::from_json(text).unwrap();
        assert_eq!(request.limit, DEFAULT_PAGE_LIMIT);
        assert!(request.since.is_none());
        assert!(request.changes.is_empty());
    }

    // ---------- 快照恢复 ----------

    #[test]
    fn log_survives_a_snapshot_round_trip() {
        let book = BookId::new();
        let (change, id) = annotation_change(book, 100, "笔记");
        let mut log = ChangeLog::new();
        log.apply(progress_change(book, 90, 0.3)).unwrap();
        log.apply(change).unwrap();

        let restored = ChangeLog::from_changes(&log.to_changes()).unwrap();
        assert_eq!(restored, log);
        assert!(restored.annotation(id).is_some());
        assert!(restored.progress_of(book).is_some());
    }

    #[test]
    fn from_changes_rejects_invalid_input() {
        let book = BookId::new();
        let mut bad = progress_change(book, 100, 0.5);
        bad.annotation = Some(AnnotationId::new());
        assert_eq!(
            ChangeLog::from_changes(&[bad]).unwrap_err().code(),
            "SYNC_TARGET_MISMATCH"
        );
    }
}
