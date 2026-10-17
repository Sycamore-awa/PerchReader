//! 同步**服务端**：持有所有人的变更日志，处理客户端的推拉请求。
//!
//! 传输层（[`crate::wire`]）定义了「一条变更长什么样、怎么分页、怎么编码」；
//! 本模块只负责**服务端语义**：把客户端上传的变更合并进一份全局日志，
//! 再把该客户端 `since` 之后的增量发回去。
//!
//! # 为什么服务端不做合并裁决
//!
//! 合并规则（HLC 全序 + 字段级 LWW）是**纯函数**，客户端和服务端用的是
//! 同一套 [`ChangeLog`](crate::ChangeLog)。服务端因此不需要自己的裁决逻辑，
//! 它只是「所有人的日志的并集」。好处是：服务端算出的结果与客户端离线重放
//! 算出的结果**必然一致**，不会出现「服务端这么合并、客户端那么合并」的两套真相。
//!
//! # 一次请求的顺序很重要
//!
//! [`SyncServer::handle`] 严格按下面的顺序执行（顺序本身就是语义）：
//!
//! 1. 校验协议版本；
//! 2. **整体**校验本批变更，不合法就整批拒绝，一条都不写入；
//! 3. 应用本批变更，得到合并统计；
//! 4. 从**应用之后**的日志里取 `since` 之后的增量；
//! 5. 用增量页与统计构造响应。
//!
//! 第 4 步必须在第 3 步之后：客户端推上去的值如果没被接受（例如被更旧的
//! 时间戳覆盖），它需要在**同一次响应**里看到服务端上真正的胜者，
//! 而不是等到下一轮才发现自己的值没生效。

use crate::wire::{ChangeLog, SyncError, SyncRequest, SyncResponse, check_protocol, validate_all};

/// 同步服务端：所有设备变更日志的并集。
///
/// 内部只持有一份 [`ChangeLog`]（每个寄存器保留当前胜者）。
/// 本类型是纯逻辑的：不做 I/O、不持有连接、不依赖时钟（时间由调用方传入），
/// 因此可以在测试里完全确定性地驱动。
#[derive(Debug, Clone, Default)]
pub struct SyncServer {
    /// 全局变更日志：所有设备的变更都合并到这里。
    log: ChangeLog,
    /// 最近一次请求带来的服务端时间（毫秒）。
    ///
    /// 本实现**不生成** HLC（变更时间戳由客户端提供），保存它只用于观测，
    /// 以及将来做墓碑过期、客户端时钟偏移检查时有个锚点。
    last_now_ms: i64,
}

impl SyncServer {
    /// 新建一个空服务端。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 处理一次同步请求：先应用客户端上传的变更，再返回 `since` 之后的增量。
    ///
    /// # 执行顺序（顺序即语义）
    ///
    /// 1. [`check_protocol`]：版本不支持直接返回 [`SyncError::UnsupportedProtocol`]。
    /// 2. [`validate_all`]：整批校验；任意一条不合法就**整批拒绝**，
    ///    一条都不写入（否则客户端会以为整批都推成功了）。
    /// 3. [`ChangeLog::apply_all`]：合并本批变更，得到 [`MergeStats`](crate::MergeStats)。
    /// 4. [`ChangeLog::changes_since`]：用 `since` + `limit` 从**应用之后**的日志取一页。
    /// 5. [`SyncResponse::from_page`]：把增量页与统计打包返回。
    ///
    /// # Errors
    ///
    /// - 版本不是本端支持的版本 → [`SyncError::UnsupportedProtocol`]
    /// - 任一条变更不合法 → 返回该条错误，且服务端状态**完全不变**
    pub fn handle(&mut self, request: SyncRequest, now_ms: i64) -> Result<SyncResponse, SyncError> {
        // 1. 版本闸门：不知道更高的协议怎么降级，直接拒绝。
        check_protocol(request.protocol)?;

        // 2. 先整体校验：apply_all 是逐条写入的，若中途失败会留下半批数据。
        //    客户端会以为整批推成功，所以必须在这里前置拦下。
        validate_all(&request.changes)?;

        // 3. 合并到全局日志。走到这里校验已通过，理论上不会失败；仍用 `?` 传播，
        //    不假设不可能的事。
        let report = self.log.apply_all(&request.changes)?;
        self.last_now_ms = now_ms;

        // 4. 取增量必须在合并之后：让客户端在同一次往返里看到自己刚推的值的最终裁决。
        let page = self
            .log
            .changes_since(request.since.as_ref(), request.limit);

        // 5. 构造响应。
        Ok(SyncResponse::from_page(page, report.stats))
    }

    /// 某个寄存器当前有多少条记录（观察用）。
    ///
    /// 注意：这是**寄存器**数量，不是历史变更数——每个寄存器只保留当前胜者。
    #[must_use]
    pub fn len(&self) -> usize {
        self.log.len()
    }

    /// 是否还没有任何寄存器。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.log.is_empty()
    }

    /// 最近一次成功请求带来的服务端时间（毫秒）。
    #[must_use]
    pub const fn last_now_ms(&self) -> i64 {
        self.last_now_ms
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::SyncServer;
    use crate::{
        Change, ChangeLog, ChangeOp, DEFAULT_PAGE_LIMIT, Hlc, PROTOCOL_VERSION, SyncError,
        SyncRequest, WireValue,
    };
    use perch_model::{Annotation, AnnotationId, BookId, Locator};

    fn stamp(wall: i64, counter: u32, device: &str) -> Hlc {
        Hlc::from_parts(wall, counter, device)
    }

    fn locator(progression: f64) -> Locator {
        Locator::new_epub("/c1.xhtml", Some("/6/4".to_owned()), Some(progression))
    }

    fn progress_change(book: BookId, wall: i64, progression: f64, device: &str) -> Change {
        Change {
            book,
            annotation: None,
            hlc: stamp(wall, 0, device),
            op: ChangeOp::set(WireValue::progress(locator(progression))),
        }
    }

    fn annotation_change(
        book: BookId,
        wall: i64,
        note: &str,
        device: &str,
    ) -> (Change, AnnotationId) {
        let hlc = stamp(wall, 0, device);
        let mut annotation = Annotation::new_note(book, locator(0.25), note);
        annotation.hlc = Some(hlc.to_string());
        let id = annotation.id;
        (
            Change {
                book,
                annotation: Some(id),
                hlc,
                op: ChangeOp::set(WireValue::annotation(annotation)),
            },
            id,
        )
    }

    fn request(
        device: &str,
        since: Option<Hlc>,
        limit: usize,
        changes: Vec<Change>,
    ) -> SyncRequest {
        SyncRequest {
            protocol: PROTOCOL_VERSION,
            device: device.to_owned(),
            since,
            limit,
            changes,
        }
    }

    #[test]
    fn new_server_is_empty() {
        let server = SyncServer::new();
        assert!(server.is_empty());
        assert_eq!(server.len(), 0);
        assert_eq!(server.last_now_ms(), 0);
    }

    #[test]
    fn empty_request_returns_empty_page() {
        let mut server = SyncServer::new();
        let response = server
            .handle(
                request("phone", None, DEFAULT_PAGE_LIMIT, Vec::new()),
                1_000,
            )
            .unwrap();
        assert!(response.changes.is_empty());
        assert!(response.next_cursor.is_none());
        assert!(!response.has_more);
        assert_eq!(response.stats, crate::MergeStats::default());
        assert!(server.is_empty());
    }

    #[test]
    fn handle_applies_then_returns_incremental() {
        let book = BookId::new();
        let mut server = SyncServer::new();
        let change = progress_change(book, 100, 0.5, "phone");

        let response = server
            .handle(
                request("phone", None, DEFAULT_PAGE_LIMIT, vec![change.clone()]),
                1_000,
            )
            .unwrap();

        assert_eq!(server.len(), 1);
        assert_eq!(server.last_now_ms(), 1_000);
        assert_eq!(response.stats.initialized, 1);
        assert_eq!(
            response.changes,
            vec![change],
            "先合并再取增量：本请求刚推上去的值必须出现在响应里"
        );
        assert!(!response.has_more);
    }

    // ---------- 核心性质：两端经服务端交换后收敛 ----------

    #[test]
    fn two_clients_converge_through_server() {
        let book = BookId::new();
        let mut server = SyncServer::new();

        let phone_progress = progress_change(book, 100, 0.2, "phone");
        let (phone_note, phone_note_id) = annotation_change(book, 120, "手机上写的", "phone");
        let pad_progress = progress_change(book, 110, 0.9, "pad");

        // 两台设备各自先离线产生本地变更。
        let mut phone = ChangeLog::new();
        phone.apply_all(&[phone_progress, phone_note]).unwrap();
        let mut pad = ChangeLog::new();
        pad.apply(pad_progress).unwrap();

        // 各自把本地状态推给服务端。
        server
            .handle(
                request("phone", None, DEFAULT_PAGE_LIMIT, phone.to_changes()),
                1_000,
            )
            .unwrap();
        server
            .handle(
                request("pad", None, DEFAULT_PAGE_LIMIT, pad.to_changes()),
                1_001,
            )
            .unwrap();

        // 再从服务端拉全量，应用回本地。
        let phone_pull = server
            .handle(
                request("phone", None, DEFAULT_PAGE_LIMIT, Vec::new()),
                1_002,
            )
            .unwrap();
        let pad_pull = server
            .handle(request("pad", None, DEFAULT_PAGE_LIMIT, Vec::new()), 1_003)
            .unwrap();
        phone.apply_all(&phone_pull.changes).unwrap();
        pad.apply_all(&pad_pull.changes).unwrap();

        assert_eq!(phone, pad, "交换后两端必须收敛到同一状态");
        assert_eq!(
            phone
                .progress_of(book)
                .and_then(|l| l.locations.progression),
            Some(0.9),
            "HLC 更晚的进度胜出"
        );
        assert_eq!(phone.annotations_of(book).len(), 1);
        assert_eq!(
            phone.annotation(phone_note_id).map(|a| a.note.as_deref()),
            Some(Some("手机上写的"))
        );
        assert_eq!(server.len(), 2, "进度与标注各一个寄存器");
    }

    // ---------- 增量游标 ----------

    #[test]
    fn incremental_cursor_does_not_repeat() {
        let first_book = BookId::new();
        let second_book = BookId::new();
        let mut server = SyncServer::new();
        server
            .handle(
                request(
                    "phone",
                    None,
                    DEFAULT_PAGE_LIMIT,
                    vec![
                        progress_change(first_book, 100, 0.1, "phone"),
                        progress_change(second_book, 200, 0.2, "phone"),
                    ],
                ),
                1,
            )
            .unwrap();

        let first = server
            .handle(request("phone", None, DEFAULT_PAGE_LIMIT, Vec::new()), 2)
            .unwrap();
        assert_eq!(first.changes.len(), 2);
        let cursor = first.next_cursor.clone().unwrap();

        // 带上上次的游标：不该再拿到任何重复数据。
        let second = server
            .handle(
                request(
                    "phone",
                    Some(cursor.clone()),
                    DEFAULT_PAGE_LIMIT,
                    Vec::new(),
                ),
                3,
            )
            .unwrap();
        assert!(second.changes.is_empty(), "带游标后不该重复返回旧数据");
        assert_eq!(
            second.next_cursor,
            Some(cursor.clone()),
            "没有新数据时游标保持不变"
        );
        assert!(!second.has_more);

        // 推一条更新的之后，游标之后只应看到这一条。
        server
            .handle(
                request(
                    "phone",
                    None,
                    DEFAULT_PAGE_LIMIT,
                    vec![progress_change(first_book, 300, 0.3, "phone")],
                ),
                4,
            )
            .unwrap();
        let third = server
            .handle(
                request("phone", Some(cursor), DEFAULT_PAGE_LIMIT, Vec::new()),
                5,
            )
            .unwrap();
        assert_eq!(third.changes.len(), 1, "只返回游标之后的增量");
        assert_eq!(third.changes[0].book, first_book);
        assert_eq!(third.changes[0].hlc, stamp(300, 0, "phone"));
    }

    // ---------- 分页 ----------

    #[test]
    fn paging_walks_whole_set_exactly_once() {
        let books: Vec<BookId> = (0..5).map(|_| BookId::new()).collect();
        let mut server = SyncServer::new();
        let changes: Vec<Change> = books
            .iter()
            .enumerate()
            .map(|(index, book)| {
                progress_change(*book, 100 + index as i64, 0.1 * index as f64, "phone")
            })
            .collect();
        server
            .handle(request("phone", None, DEFAULT_PAGE_LIMIT, changes), 1)
            .unwrap();

        let mut seen: Vec<BookId> = Vec::new();
        let mut cursor: Option<Hlc> = None;
        let mut rounds = 0_u32;
        loop {
            rounds += 1;
            assert!(rounds < 20, "分页应当收敛");
            let response = server
                .handle(request("phone", cursor.clone(), 2, Vec::new()), 2)
                .unwrap();
            if rounds == 1 {
                assert!(response.has_more, "limit 小于总数时 has_more 应为真");
            }
            seen.extend(response.changes.iter().map(|change| change.book));
            cursor = response.next_cursor.clone();
            if !response.has_more {
                break;
            }
        }

        assert_eq!(seen.len(), 5, "逐页走完必须不漏不重");
        seen.sort();
        let mut expected = books;
        expected.sort();
        assert_eq!(seen, expected, "每本书恰好出现一次");
    }

    // ---------- 错误路径 ----------

    #[test]
    fn unsupported_protocol_is_rejected() {
        let mut server = SyncServer::new();

        let mut too_new = SyncRequest::new("phone");
        too_new.protocol = PROTOCOL_VERSION + 1;
        let error = server.handle(too_new, 1).unwrap_err();
        assert_eq!(
            error,
            SyncError::UnsupportedProtocol {
                got: PROTOCOL_VERSION + 1,
                max: PROTOCOL_VERSION,
            }
        );
        assert!(server.is_empty(), "被拒绝的请求不能留下状态");

        let mut zero = SyncRequest::new("phone");
        zero.protocol = 0;
        assert_eq!(
            server.handle(zero, 2).unwrap_err().code(),
            "SYNC_UNSUPPORTED_PROTOCOL"
        );
        assert!(server.is_empty());
    }

    #[test]
    fn invalid_change_rejects_whole_batch_without_writing() {
        let book = BookId::new();
        let other_book = BookId::new();
        let mut server = SyncServer::new();
        server
            .handle(
                request(
                    "phone",
                    None,
                    DEFAULT_PAGE_LIMIT,
                    vec![progress_change(book, 100, 0.5, "phone")],
                ),
                1,
            )
            .unwrap();
        assert_eq!(server.len(), 1);

        // 第一条合法（且会新建一个寄存器），第二条不合法。
        let valid_new_register = progress_change(other_book, 200, 0.9, "phone");
        let mut invalid = progress_change(book, 300, 1.0, "phone");
        invalid.annotation = Some(AnnotationId::new());

        let error = server
            .handle(
                request(
                    "phone",
                    None,
                    DEFAULT_PAGE_LIMIT,
                    vec![valid_new_register, invalid],
                ),
                2,
            )
            .unwrap_err();
        assert_eq!(error.code(), "SYNC_TARGET_MISMATCH");
        assert_eq!(server.len(), 1, "整批拒绝：合法的那条也不能留下痕迹");

        let pull = server
            .handle(request("phone", None, DEFAULT_PAGE_LIMIT, Vec::new()), 3)
            .unwrap();
        assert_eq!(pull.changes.len(), 1);
        assert_eq!(pull.changes[0].hlc, stamp(100, 0, "phone"));
    }

    // ---------- 幂等与冲突统计 ----------

    #[test]
    fn replaying_same_changes_is_idempotent() {
        let book = BookId::new();
        let mut server = SyncServer::new();
        let changes = vec![progress_change(book, 100, 0.5, "phone")];
        server
            .handle(
                request("phone", None, DEFAULT_PAGE_LIMIT, changes.clone()),
                1,
            )
            .unwrap();

        let again = server
            .handle(request("phone", None, DEFAULT_PAGE_LIMIT, changes), 2)
            .unwrap();
        assert_eq!(again.stats.unchanged, 1, "重复推同一批应计入 unchanged");
        assert_eq!(again.stats.conflicts, 0, "重复推不产生冲突");
        assert_eq!(again.stats.conflict_rate(), 0.0);
        assert_eq!(server.len(), 1);
    }

    #[test]
    fn same_stamp_different_value_is_a_conflict() {
        let book = BookId::new();
        let mut server = SyncServer::new();
        server
            .handle(
                request(
                    "phone",
                    None,
                    DEFAULT_PAGE_LIMIT,
                    vec![progress_change(book, 100, 0.3, "phone")],
                ),
                1,
            )
            .unwrap();

        // 完全相同的 HLC，但值不同 → 真正的冲突。
        let response = server
            .handle(
                request(
                    "phone",
                    None,
                    DEFAULT_PAGE_LIMIT,
                    vec![progress_change(book, 100, 0.7, "phone")],
                ),
                2,
            )
            .unwrap();
        assert_eq!(response.stats.conflicts, 1);
        assert!(response.stats.conflict_rate() > 0.0);
        assert_eq!(server.len(), 1, "冲突只裁决，不新建寄存器");

        // 保留先到的值以保持确定性。
        let pull = server
            .handle(request("phone", None, DEFAULT_PAGE_LIMIT, Vec::new()), 3)
            .unwrap();
        let progression = match &pull.changes[0].op {
            ChangeOp::Set { value } => value.as_progress().and_then(|at| at.locations.progression),
            ChangeOp::Tombstone => None,
        };
        assert_eq!(progression, Some(0.3));
    }
}
