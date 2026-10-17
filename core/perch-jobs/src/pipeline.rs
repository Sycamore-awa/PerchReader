//! 把调度器（[`perch_pipeline::Scheduler`]）和执行者（本 crate 的
//! [`refine`] / [`extract_text`] / [`index_book`]）接起来的**驱动循环**。
//!
//! # 为什么这段逻辑必须在 Rust 里
//!
//! "先做②还是先做③""失败了退避多久""并发几个"这些规则全都已经写成了
//! [`perch_pipeline`] 的可测逻辑。如果驱动循环放在 Swift 里，它就变成
//! **无法回归的胶水**：调度器有测试，但"谁按什么节奏调用它"没人测。
//!
//! 所以这里定义一个最小的存储接口 [`BookStore`]，由平台侧实现（读写沙盒 +
//! 落 SQLite），循环本身留在 Rust：一次 [`Pipeline::step`] 只做**一个**任务，
//! 因此可以在任意时刻被打断（App 进后台、用户开始阅读）。
//!
//! # 状态流转
//!
//! ```text
//! registered --②success--> refining --②done--> indexing --③done--> ready
//!      |                       |                    |
//!      +-------- 任一阶段失败 --> failed <----------+
//! ```
//!
//! 每一步都通过 [`BookStore::set_state`] 落库，**先落库再进下一步**：
//! 崩溃发生在两者之间时，重启后看到的是"上一步已完成"，会重做当前步——
//! ②和③都是幂等的，重做安全。

use perch_model::{BookId, Format, ImportState};
use perch_pipeline::{Gate, Policy, Priority, Scheduler, Stage};

use crate::{BookText, ExtractOptions, JobsError, Refinement, extract_text, index_book, refine};

/// 平台侧需要提供的存储能力。
///
/// 故意定得很窄：只包含驱动循环真正需要的四件事。
/// 错误用 `String` 而不是具体错误类型，因为这一层的失败原因只用于日志与上报。
pub trait BookStore {
    /// 读出这本书的原始字节（平台从沙盒读）。
    fn read_bytes(&mut self, book: BookId) -> Result<Vec<u8>, String>;

    /// 这本书的格式。
    ///
    /// **必须由调用方给出，不要靠嗅探**：一个损坏的 `.epub` 若被嗅探成
    /// 纯文本，会被"成功"登记成一本乱码书——静默产出一个坏结果，
    /// 比明确报错糟糕得多。导入阶段从扩展名已经知道格式，直接给出来即可。
    fn format(&mut self, book: BookId) -> Result<Format, String>;

    /// 更新导入状态。
    fn set_state(&mut self, book: BookId, state: ImportState) -> Result<(), String>;

    /// 保存阶段②的产物。
    fn save_refinement(&mut self, book: BookId, refinement: &Refinement) -> Result<(), String>;

    /// 保存阶段③的产物（索引 + 正文/章节映射）。
    ///
    /// 索引按值移动而不是引用：平台大概率要把它落盘，拿走所有权更自然。
    fn save_index(
        &mut self,
        book: BookId,
        index: perch_index::Index,
        text: BookText,
    ) -> Result<(), String>;
}

/// 一次 [`Pipeline::step`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// 没有可做的任务（队列空，或被闸门挡住）。
    Idle,
    /// 阶段②完成。
    Refined {
        /// 书。
        book: BookId,
    },
    /// 阶段③完成。
    Indexed {
        /// 书。
        book: BookId,
        /// 分片数。
        chunks: usize,
        /// 倒排里不同 token 的数量。
        tokens: usize,
    },
    /// 某一步失败，已安排退避重试。
    Failed {
        /// 书。
        book: BookId,
        /// 失败的阶段。
        stage: Stage,
        /// 原因。
        error: String,
        /// 这是同类失败的第几次。
        attempts: u16,
    },
    /// 重试次数用尽，等人工处理。
    Exhausted {
        /// 书。
        book: BookId,
        /// 失败的阶段。
        stage: Stage,
    },
}

/// 驱动循环。
pub struct Pipeline<S: BookStore> {
    scheduler: Scheduler,
    store: S,
    options: ExtractOptions,
    chunk_chars: u64,
}

impl<S: BookStore> Pipeline<S> {
    /// 用给定策略构造。
    ///
    /// # Errors
    ///
    /// 策略不自洽时返回 [`perch_pipeline::PipelineError`]。
    pub fn new(
        store: S,
        policy: Policy,
        options: ExtractOptions,
        chunk_chars: u64,
    ) -> Result<Self, perch_pipeline::PipelineError> {
        Ok(Self {
            scheduler: Scheduler::new(policy)?,
            store,
            options,
            chunk_chars,
        })
    }

    /// 登记一本书（阶段①完成后调用）。
    pub fn enqueue(&mut self, book: BookId, state: ImportState, priority: Priority) {
        self.scheduler.enqueue(book, state, priority);
    }

    /// 用户打开某本书：插队。
    pub fn promote(&mut self, book: BookId, priority: Priority) {
        self.scheduler.promote(book, priority);
    }

    /// 删书：取消所有相关任务。
    pub fn cancel(&mut self, book: BookId) {
        self.scheduler.cancel(book);
    }

    /// 借用调度器（查统计、看队列）。
    #[must_use]
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// 借用存储。
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// 队列空了吗。
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.scheduler.is_idle()
    }

    /// 做**一个**任务。
    ///
    /// 刻意一次只做一个：调用方（后台线程 / 前台帧间）可以自由决定节奏，
    /// 也能在任意两次调用之间被打断。
    pub fn step(&mut self, now_ms: u64, gate: Gate) -> StepOutcome {
        let Some(assignment) = self.scheduler.next(now_ms, gate) else {
            return StepOutcome::Idle;
        };
        let book = assignment.book;
        let stage = assignment.stage;

        match stage {
            Stage::Refine => self.run_refine(book, now_ms),
            Stage::Index => self.run_index(book, now_ms),
        }
    }

    /// 一直做到没有任务为止。仅供测试与"前台空闲时跑一跑"使用。
    ///
    /// ⚠️ 全程用**同一个** `now_ms`，因此**不会**触发退避重试（退避要看时间前进）。
    /// 想验证重试/耗尽，请自己循环调用 [`Pipeline::step`] 并推进时钟。
    ///
    /// 返回每种结果出现的次数，顺序为 `(refined, indexed, failed, exhausted)`。
    pub fn run_until_idle(&mut self, now_ms: u64, gate: Gate, max_steps: usize) -> StepSummary {
        let mut summary = StepSummary::default();
        for _ in 0..max_steps {
            match self.step(now_ms, gate) {
                StepOutcome::Idle => break,
                StepOutcome::Refined { .. } => summary.refined += 1,
                StepOutcome::Indexed { .. } => summary.indexed += 1,
                StepOutcome::Failed { .. } => summary.failed += 1,
                StepOutcome::Exhausted { .. } => summary.exhausted += 1,
            }
        }
        summary
    }

    fn run_refine(&mut self, book: BookId, now_ms: u64) -> StepOutcome {
        // 先落库"正在精修"，崩溃后才看得出这一步没做完
        if let Err(error) = self.store.set_state(book, ImportState::Refining) {
            return self.fail(book, Stage::Refine, error, now_ms);
        }
        let bytes = match self.store.read_bytes(book) {
            Ok(bytes) => bytes,
            Err(error) => return self.fail(book, Stage::Refine, error, now_ms),
        };
        let format = match self.store.format(book) {
            Ok(format) => format,
            Err(error) => return self.fail(book, Stage::Refine, error, now_ms),
        };
        let refinement = match refine(&bytes, format, self.options) {
            Ok(refinement) => refinement,
            Err(error) => return self.fail(book, Stage::Refine, describe(&error), now_ms),
        };
        if let Err(error) = self.store.save_refinement(book, &refinement) {
            return self.fail(book, Stage::Refine, error, now_ms);
        }
        // ②成功 → 状态推进到 indexing，并把③排进队列
        if let Err(error) = self.store.set_state(book, ImportState::Indexing) {
            return self.fail(book, Stage::Refine, error, now_ms);
        }
        self.scheduler.complete(book);
        // ③统一按 Normal 排队：②决定"书架上能不能看到信息"，
        // 是用户感知的瓶颈；③只是搜索可用性，不值得为它插队。
        self.scheduler
            .enqueue(book, ImportState::Indexing, Priority::Normal);
        StepOutcome::Refined { book }
    }

    fn run_index(&mut self, book: BookId, now_ms: u64) -> StepOutcome {
        let bytes = match self.store.read_bytes(book) {
            Ok(bytes) => bytes,
            Err(error) => return self.fail(book, Stage::Index, error, now_ms),
        };
        let format = match self.store.format(book) {
            Ok(format) => format,
            Err(error) => return self.fail(book, Stage::Index, error, now_ms),
        };
        let text = match extract_text(&bytes, format, self.options) {
            Ok(text) => text,
            Err(error) => return self.fail(book, Stage::Index, describe(&error), now_ms),
        };
        let index = index_book(&text, self.chunk_chars);
        let stats = index.stats();
        if let Err(error) = self.store.save_index(book, index, text) {
            return self.fail(book, Stage::Index, error, now_ms);
        }
        if let Err(error) = self.store.set_state(book, ImportState::Ready) {
            return self.fail(book, Stage::Index, error, now_ms);
        }
        self.scheduler.complete(book);
        StepOutcome::Indexed {
            book,
            chunks: stats.chunks,
            tokens: stats.tokens,
        }
    }

    fn fail(&mut self, book: BookId, stage: Stage, error: String, now_ms: u64) -> StepOutcome {
        // 先尽力把 failed 落库；即使这一步也失败，调度器仍然会退避重试
        let _ = self.store.set_state(book, ImportState::Failed);
        match self.scheduler.fail(book, now_ms) {
            perch_pipeline::FailOutcome::Exhausted => StepOutcome::Exhausted { book, stage },
            perch_pipeline::FailOutcome::RetryScheduled { attempts, .. } => StepOutcome::Failed {
                book,
                stage,
                error,
                attempts,
            },
            perch_pipeline::FailOutcome::NotRunning => StepOutcome::Failed {
                book,
                stage,
                error,
                attempts: 0,
            },
        }
    }
}

/// [`Pipeline::run_until_idle`] 的汇总。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StepSummary {
    /// ②完成次数。
    pub refined: u32,
    /// ③完成次数。
    pub indexed: u32,
    /// 失败（已安排重试）次数。
    pub failed: u32,
    /// 重试耗尽的次数。
    pub exhausted: u32,
}

fn describe(error: &JobsError) -> String {
    format!("{}: {error}", error.code())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::tests::build_epub;
    use perch_model::Format;
    use std::collections::HashMap;

    /// 内存存储：记录所有调用，便于断言"先落库再进下一步"。
    #[derive(Default)]
    struct FakeStore {
        files: HashMap<BookId, Vec<u8>>,
        states: Vec<(BookId, ImportState)>,
        refinements: Vec<Refinement>,
        indexes: Vec<(usize, usize)>,
        fail_read: bool,
        fail_save_refinement: bool,
        formats: HashMap<BookId, Format>,
    }

    impl BookStore for FakeStore {
        fn read_bytes(&mut self, book: BookId) -> Result<Vec<u8>, String> {
            if self.fail_read {
                return Err("读不到文件".to_owned());
            }
            self.files
                .get(&book)
                .cloned()
                .ok_or_else(|| "文件不存在".to_owned())
        }

        fn format(&mut self, book: BookId) -> Result<Format, String> {
            // 默认 EPUB；测试可以按书覆盖
            Ok(self.formats.get(&book).copied().unwrap_or(Format::Epub))
        }

        fn set_state(&mut self, book: BookId, state: ImportState) -> Result<(), String> {
            self.states.push((book, state));
            Ok(())
        }

        fn save_refinement(
            &mut self,
            _book: BookId,
            refinement: &Refinement,
        ) -> Result<(), String> {
            if self.fail_save_refinement {
                return Err("落库失败".to_owned());
            }
            self.refinements.push(refinement.clone());
            Ok(())
        }

        fn save_index(
            &mut self,
            _book: BookId,
            index: perch_index::Index,
            _text: BookText,
        ) -> Result<(), String> {
            let stats = index.stats();
            self.indexes.push((stats.chunks, stats.tokens));
            Ok(())
        }
    }

    fn pipeline_with(store: FakeStore) -> Pipeline<FakeStore> {
        Pipeline::new(store, Policy::default(), ExtractOptions::default(), 64).unwrap()
    }

    fn store_with_book(book: BookId) -> FakeStore {
        let mut store = FakeStore::default();
        store
            .files
            .insert(book, build_epub(&["春天的花开", "夏天的雨落"], true, false));
        store
    }

    // ---------- 正常路径 ----------

    #[test]
    fn book_runs_through_both_stages_to_ready() {
        let book = BookId::new();
        let mut pipeline = pipeline_with(store_with_book(book));
        pipeline.enqueue(book, ImportState::Registered, Priority::Normal);

        let summary = pipeline.run_until_idle(0, Gate::foreground(), 20);
        assert_eq!(summary.refined, 1);
        assert_eq!(summary.indexed, 1);
        assert_eq!(summary.failed, 0);
        assert!(pipeline.is_idle());

        let store = pipeline.store();
        // 状态按顺序落库：正在精修 → 索引中 → 可用
        assert_eq!(
            store.states,
            vec![
                (book, ImportState::Refining),
                (book, ImportState::Indexing),
                (book, ImportState::Ready),
            ]
        );
        assert_eq!(store.refinements.len(), 1);
        assert_eq!(store.refinements[0].title.as_deref(), Some("测试书名"));
        assert_eq!(store.indexes.len(), 1);
        assert!(store.indexes[0].0 >= 1, "至少一个分片");
    }

    #[test]
    fn step_returns_indexed_outcome_with_stats() {
        let book = BookId::new();
        let mut pipeline = pipeline_with(store_with_book(book));
        pipeline.enqueue(book, ImportState::Registered, Priority::Normal);

        // 第一步是精修
        assert_eq!(
            pipeline.step(0, Gate::foreground()),
            StepOutcome::Refined { book }
        );
        // 第二步是建索引
        match pipeline.step(1, Gate::foreground()) {
            StepOutcome::Indexed {
                book: id,
                chunks,
                tokens,
            } => {
                assert_eq!(id, book);
                assert!(chunks >= 1);
                assert!(tokens >= 1);
            }
            other => panic!("第二步应当是 Indexed，实际 {other:?}"),
        }
        // 之后没有任务了
        assert_eq!(pipeline.step(2, Gate::foreground()), StepOutcome::Idle);
    }

    #[test]
    fn two_books_both_finish() {
        let a = BookId::new();
        let b = BookId::new();
        let mut store = store_with_book(a);
        store
            .files
            .insert(b, build_epub(&["另一本书的内容"], false, false));
        let mut pipeline = pipeline_with(store);
        pipeline.enqueue(a, ImportState::Registered, Priority::Normal);
        pipeline.enqueue(b, ImportState::Registered, Priority::Normal);

        let summary = pipeline.run_until_idle(0, Gate::foreground(), 40);
        assert_eq!(summary.refined, 2);
        assert_eq!(summary.indexed, 2);
        assert!(pipeline.is_idle());
    }

    // ---------- 闸门 ----------

    #[test]
    fn suspended_gate_does_nothing_at_all() {
        let book = BookId::new();
        let mut pipeline = pipeline_with(store_with_book(book));
        pipeline.enqueue(book, ImportState::Registered, Priority::Normal);

        assert_eq!(pipeline.step(0, Gate::suspended()), StepOutcome::Idle);
        assert!(pipeline.store().states.is_empty(), "不该有任何落库");
        assert!(!pipeline.is_idle(), "任务仍然在队列里");
    }

    // ---------- 失败路径 ----------

    #[test]
    fn read_failure_marks_failed_then_retries() {
        let book = BookId::new();
        let mut store = store_with_book(book);
        store.fail_read = true;
        let mut pipeline = pipeline_with(store);
        pipeline.enqueue(book, ImportState::Registered, Priority::Normal);

        match pipeline.step(0, Gate::foreground()) {
            StepOutcome::Failed {
                book: id,
                stage,
                attempts,
                ..
            } => {
                assert_eq!(id, book);
                assert_eq!(stage, Stage::Refine);
                assert_eq!(attempts, 1);
            }
            other => panic!("应当失败，实际 {other:?}"),
        }
        // 失败状态必须落库
        assert_eq!(
            pipeline.store().states.last(),
            Some(&(book, ImportState::Failed))
        );
        // 退避窗口内不再调度
        assert_eq!(pipeline.step(1, Gate::foreground()), StepOutcome::Idle);
    }

    #[test]
    fn corrupt_epub_fails_the_stage() {
        let book = BookId::new();
        let mut store = FakeStore::default();
        store.files.insert(book, "这不是 epub".as_bytes().to_vec());
        let mut pipeline = pipeline_with(store);
        pipeline.enqueue(book, ImportState::Registered, Priority::Normal);

        match pipeline.step(0, Gate::foreground()) {
            StepOutcome::Failed { stage, error, .. } => {
                assert_eq!(stage, Stage::Refine);
                assert!(error.contains("JOBS_EPUB"), "错误应带稳定错误码: {error}");
            }
            other => panic!("应当失败，实际 {other:?}"),
        }
    }

    #[test]
    fn persistence_failure_fails_the_stage() {
        let book = BookId::new();
        let mut store = store_with_book(book);
        store.fail_save_refinement = true;
        let mut pipeline = pipeline_with(store);
        pipeline.enqueue(book, ImportState::Registered, Priority::Normal);

        match pipeline.step(0, Gate::foreground()) {
            StepOutcome::Failed { stage, error, .. } => {
                assert_eq!(stage, Stage::Refine);
                assert_eq!(error, "落库失败");
            }
            other => panic!("应当失败，实际 {other:?}"),
        }
    }

    #[test]
    fn retries_exhaust_after_max_attempts() {
        let book = BookId::new();
        let mut store = store_with_book(book);
        store.fail_read = true;
        let mut pipeline = pipeline_with(store);
        pipeline.enqueue(book, ImportState::Registered, Priority::Normal);

        // 每次失败后推进时钟越过退避窗口，直到耗尽（默认最多 3 次）
        let mut now = 0u64;
        let mut last = StepOutcome::Idle;
        for _ in 0..10 {
            last = pipeline.step(now, Gate::foreground());
            match &last {
                StepOutcome::Exhausted { .. } => break,
                StepOutcome::Failed { .. } => now += 120_000,
                _ => panic!("预期失败或耗尽，实际 {last:?}"),
            }
        }
        assert!(
            matches!(
                last,
                StepOutcome::Exhausted {
                    stage: Stage::Refine,
                    ..
                }
            ),
            "重试应当耗尽，实际 {last:?}"
        );
        assert!(pipeline.is_idle());
    }

    #[test]
    fn cancel_removes_pending_work() {
        let book = BookId::new();
        let mut pipeline = pipeline_with(store_with_book(book));
        pipeline.enqueue(book, ImportState::Registered, Priority::Normal);
        pipeline.cancel(book);
        assert!(pipeline.is_idle());
        assert_eq!(pipeline.step(0, Gate::foreground()), StepOutcome::Idle);
    }
    #[test]
    fn txt_book_runs_through_both_stages() {
        let book = BookId::new();
        let mut store = FakeStore::default();
        store
            .files
            .insert(book, "第一章\r\n\r\n\r\n中文纯文本内容".as_bytes().to_vec());
        store.formats.insert(book, Format::Txt);
        let mut pipeline = pipeline_with(store);
        pipeline.enqueue(book, ImportState::Registered, Priority::Normal);

        let summary = pipeline.run_until_idle(0, Gate::foreground(), 20);
        assert_eq!(summary.refined, 1);
        assert_eq!(summary.indexed, 1);
        assert_eq!(
            pipeline.store().states,
            vec![
                (book, ImportState::Refining),
                (book, ImportState::Indexing),
                (book, ImportState::Ready),
            ]
        );
        // 编码探测结果应当被落库（TXT 特有）
        let encoding = pipeline.store().refinements[0].encoding.clone();
        assert!(encoding.is_some(), "TXT 精修应带上探测到的编码");
    }

    #[test]
    fn pdf_book_fails_with_unsupported_format() {
        let book = BookId::new();
        let mut store = FakeStore::default();
        store.files.insert(book, b"%PDF-1.7".to_vec());
        store.formats.insert(book, Format::Pdf);
        let mut pipeline = pipeline_with(store);
        pipeline.enqueue(book, ImportState::Registered, Priority::Normal);

        match pipeline.step(0, Gate::foreground()) {
            StepOutcome::Failed { stage, error, .. } => {
                assert_eq!(stage, Stage::Refine);
                assert!(
                    error.contains("JOBS_UNSUPPORTED_FORMAT"),
                    "应当报不支持的格式，实际 {error}"
                );
            }
            other => panic!("PDF 应当失败，实际 {other:?}"),
        }
    }
}
