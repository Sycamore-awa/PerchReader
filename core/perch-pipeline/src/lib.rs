//! 导入管线**阶段②③的调度**（`docs/01` 的三阶段设计）。
//!
//! 阶段①（登记）在 [`perch_import`] 里，前台同步完成；②（精修元数据/封面/目录）
//! 与③（建全文索引）都太慢，必须放到后台。本 crate 只做**纯调度决策**：
//!
//! - 不持有线程、不做 I/O、不读系统时钟（`now_ms` 由调用方传入）
//! - 不依赖任何平台 API，因此**可以在 Linux 上完整测试**，再原样给 iOS 用
//! - 决策结果只是一串 [`Assignment`]，真正的活儿由调用方（iOS 的后台队列）去干
//!
//! # 为什么调度要单独成 crate
//!
//! "1000 本 <10s 上架"依赖的不只是①阶段快，还依赖②③**不和新导入抢资源**、
//! **能被随时叫停**（用户开始阅读时要立刻让路）、**崩溃后能接着做且不重复**。
//! 这些规则如果不落到可测的纯逻辑里，就只能散落在 Swift 的异步代码中，无法回归。
//!
//! # 用法
//!
//! ```text
//! let mut sched = Scheduler::new(Policy::default())?;
//! for book in newly_imported { sched.enqueue(book, ImportState::Registered, Priority::Normal); }
//! loop {
//!     for a in sched.tick(now_ms(), gate) {          // 取走「现在该干的任务」
//!         match run(a) {
//!             Ok(())    => { sched.complete(a.book); }
//!             Err(e)    => { sched.fail(a.book, now_ms()); }
//!             Err(Budget) => { sched.defer(a.book, now_ms()); }  // 没干完，不计失败
//!         }
//!     }
//! }
//! ```
//!
//! # 崩溃恢复
//!
//! 调度器**不持久化**——真正的状态源是数据库里的 `Book.import_state`。
//! 重启后按库里的记录重建：[`Scheduler::resume`] 把 `refining` / `indexing`
//! 的书重新排进队列（这正是"杀进程不产生重复条目"的实现方式：不另存队列，
//! 队列是库的**派生视图**）。

#![forbid(unsafe_code)]

use perch_model::{BookId, ImportState};

/// 调度相关的错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PipelineError {
    /// 两个并发上限都是 0，调度器永远不会派活。
    #[error("并发上限不能同时为 0")]
    NoConcurrency,
    /// `max_attempts` 为 0 时任务一旦失败就无处可去。
    #[error("max_attempts 必须 >= 1")]
    ZeroAttempts,
    /// 退避基数大于上限，指数退避无从收敛。
    #[error("backoff_base_ms 不能大于 backoff_max_ms")]
    BackoffInverted,
}

/// 后台阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// 阶段②：解析元数据 / 封面 / 目录 / 字数（单本 50~300ms）。
    Refine,
    /// 阶段③：建全文倒排索引（单本 0.5~5s）。
    Index,
}

impl Stage {
    /// 稳定的字符串表示（用于日志与进度上报）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refine => "refine",
            Self::Index => "index",
        }
    }

    /// 该阶段是否属于"重型"工作（受 [`Gate::allow_heavy`] 管制）。
    #[must_use]
    pub const fn is_heavy(self) -> bool {
        matches!(self, Self::Index)
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 任务优先级。**`rank()` 越小越紧急**。
///
/// `derive(Ord)` 的声明顺序与 [`Priority::rank`] 一致（`Urgent` 最小），
/// 因此"排序后取第一个"就是"取最紧急的"。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// 用户正在打开、或马上就要打开的书。必须无条件插队。
    Urgent,
    /// 刚导入、书架上看得见的书。
    #[default]
    Normal,
    /// 已经滚出屏幕、或用户很久没碰的书。
    Background,
}

impl Priority {
    /// 排序权重，**越小越先做**。
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Urgent => 0,
            Self::Normal => 1,
            Self::Background => 2,
        }
    }

    /// `self` 是否比 `other` 紧急。
    #[must_use]
    pub const fn is_more_urgent_than(self, other: Self) -> bool {
        self.rank() < other.rank()
    }
}

/// 外部条件闸门：由平台侧根据 App 生命周期 / 低电量模式 / 热节流设置。
///
/// 调度器不认识"前台""后台"这些概念，只认这两个布尔值，
/// 这样规则可以在 Linux 上测，平台差异全留在 Swift 侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate {
    /// 是否允许任何后台工作。App 被挂起且没有后台时间片时为 `false`。
    pub allow_any: bool,
    /// 是否允许重型工作（阶段③）。低电量或热节流时置 `false`，只做②。
    pub allow_heavy: bool,
}

impl Gate {
    /// 前台：全速。
    #[must_use]
    pub const fn foreground() -> Self {
        Self {
            allow_any: true,
            allow_heavy: true,
        }
    }

    /// 有后台时间片：可以干活，但仍受并发上限约束。
    #[must_use]
    pub const fn background() -> Self {
        Self {
            allow_any: true,
            allow_heavy: true,
        }
    }

    /// 受限：只做轻量的阶段②，不启动索引。
    #[must_use]
    pub const fn throttled() -> Self {
        Self {
            allow_any: true,
            allow_heavy: false,
        }
    }

    /// 完全暂停。
    #[must_use]
    pub const fn suspended() -> Self {
        Self {
            allow_any: false,
            allow_heavy: false,
        }
    }

    /// 该闸门是否放行某个阶段。
    #[must_use]
    pub const fn permits(self, stage: Stage) -> bool {
        if !self.allow_any {
            return false;
        }
        if stage.is_heavy() && !self.allow_heavy {
            return false;
        }
        true
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self::foreground()
    }
}

/// 并发与重试策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// 阶段②同时进行的上限。②是轻量 I/O + 解析，可以并行。
    pub max_refine: usize,
    /// 阶段③同时进行的上限。③吃 CPU 和内存，**默认串行**，避免把主线程挤掉。
    pub max_index: usize,
    /// 单本最多尝试几次（含首次）。达到上限后转入 `exhausted`，等用户手动重试。
    pub max_attempts: u16,
    /// 首次失败的退避时长。
    pub backoff_base_ms: u64,
    /// 退避上限。
    pub backoff_max_ms: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_refine: 2,
            max_index: 1,
            max_attempts: 3,
            backoff_base_ms: 2_000,
            backoff_max_ms: 60_000,
        }
    }
}

impl Policy {
    /// 策略自洽性检查。`Scheduler::new` 会调用它，避免构造出永远不干活或有活干不完的调度器。
    pub fn validate(self) -> Result<(), PipelineError> {
        if self.max_refine == 0 && self.max_index == 0 {
            return Err(PipelineError::NoConcurrency);
        }
        if self.max_attempts == 0 {
            return Err(PipelineError::ZeroAttempts);
        }
        if self.backoff_base_ms > self.backoff_max_ms {
            return Err(PipelineError::BackoffInverted);
        }
        Ok(())
    }
}

/// `enqueue` / `resume` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// 已入队，等待该阶段被调度。
    Queued(Stage),
    /// 该书已在队列中或正在运行；**幂等**，不产生重复任务。
    AlreadyPresent,
    /// 该状态不需要后台工作（`ready`），或失败需要显式重试（`failed`）。
    NotNeeded,
}

/// 排队中的任务。字段只读，外部不得直接改。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// 书。
    pub book: BookId,
    /// 待做的阶段。
    pub stage: Stage,
    /// 优先级。
    pub priority: Priority,
    /// 已失败次数。
    pub attempts: u16,
    /// 早于此时刻（毫秒）不调度。
    pub not_before_ms: u64,
    /// 入队序号，用于同优先级下的 FIFO。
    pub seq: u64,
}

/// 正在运行的任务。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InFlight {
    /// 书。
    pub book: BookId,
    /// 正在做的阶段。
    pub stage: Stage,
    /// 优先级。
    pub priority: Priority,
    /// 之前的失败次数。
    pub attempts: u16,
    /// 开始时刻（毫秒）。
    pub started_ms: u64,
}

/// 一次「现在该做这个」的指派。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assignment {
    /// 书。
    pub book: BookId,
    /// 要做的阶段。
    pub stage: Stage,
}

/// 重试耗尽、等用户手动处理的任务。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exhausted {
    /// 书。
    pub book: BookId,
    /// 最后失败的阶段。
    pub stage: Stage,
    /// 总共失败次数。
    pub attempts: u16,
}

/// `fail` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    /// 该书并不在运行中（已取消 / 已完结 / 从未开始）。
    NotRunning,
    /// 已安排退避重试。
    RetryScheduled {
        /// 失败后累计的次数。
        attempts: u16,
        /// 最早可以重试的时刻（毫秒）。
        not_before_ms: u64,
    },
    /// 尝试次数用尽，转入 [`Scheduler::exhausted`]。
    Exhausted,
}

/// 队列快照，用于进度上报（`docs/01` 的"正在精修 N 本"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// 排队中、等待做阶段②的书数。
    pub pending_refine: usize,
    /// 排队中、等待做阶段③的书数。
    pub pending_index: usize,
    /// 正在运行的任务数。
    pub in_flight: usize,
    /// 重试耗尽的任务数。
    pub exhausted: usize,
}

/// 阶段②③的调度器。纯内存、纯逻辑、单线程。
#[derive(Debug, Clone)]
pub struct Scheduler {
    policy: Policy,
    pending: Vec<Task>,
    running: Vec<InFlight>,
    exhausted: Vec<Exhausted>,
    next_seq: u64,
}

impl Scheduler {
    /// 用给定策略构造。策略不自洽时返回错误（不会 panic）。
    pub fn new(policy: Policy) -> Result<Self, PipelineError> {
        policy.validate()?;
        Ok(Self {
            policy,
            pending: Vec::new(),
            running: Vec::new(),
            exhausted: Vec::new(),
            next_seq: 0,
        })
    }

    /// 当前策略。
    #[must_use]
    pub const fn policy(&self) -> Policy {
        self.policy
    }

    /// 由导入状态推导**下一步**该做哪个后台阶段。
    ///
    /// - `registered` / `refining` → ②（中途被杀就重做②，②是幂等的）
    /// - `indexing` → ③
    /// - `ready` → 无需后台工作
    /// - `failed` → `None`：失败要显式重试，避免重启后无限自动重试
    #[must_use]
    pub const fn stage_for(state: ImportState) -> Option<Stage> {
        match state {
            ImportState::Registered | ImportState::Refining => Some(Stage::Refine),
            ImportState::Indexing => Some(Stage::Index),
            ImportState::Ready | ImportState::Failed => None,
        }
    }

    /// 书是否已在队列里或正在运行。
    #[must_use]
    pub fn contains(&self, book: BookId) -> bool {
        self.pending.iter().any(|t| t.book == book) || self.running.iter().any(|r| r.book == book)
    }

    /// 入队一本书（新导入的路径，尝试次数从 0 开始）。
    pub fn enqueue(
        &mut self,
        book: BookId,
        state: ImportState,
        priority: Priority,
    ) -> EnqueueOutcome {
        self.push(book, state, priority, 0)
    }

    /// 启动时按数据库记录重建（崩溃恢复路径）。
    ///
    /// `attempts` 应当来自持久化，否则重启会把失败次数清零、无限重试。
    pub fn resume(
        &mut self,
        book: BookId,
        state: ImportState,
        priority: Priority,
        attempts: u16,
    ) -> EnqueueOutcome {
        self.push(book, state, priority, attempts)
    }

    fn push(
        &mut self,
        book: BookId,
        state: ImportState,
        priority: Priority,
        attempts: u16,
    ) -> EnqueueOutcome {
        if self.contains(book) {
            return EnqueueOutcome::AlreadyPresent;
        }
        let Some(stage) = Self::stage_for(state) else {
            return EnqueueOutcome::NotNeeded;
        };
        self.pending.push(Task {
            book,
            stage,
            priority,
            attempts,
            not_before_ms: 0,
            seq: self.next_seq,
        });
        self.next_seq = self.next_seq.saturating_add(1);
        EnqueueOutcome::Queued(stage)
    }

    /// 取走下一个该做的任务；没有就返回 `None`。
    ///
    /// 一次只派一个，调用方循环使用 [`Scheduler::tick`]。
    pub fn next(&mut self, now_ms: u64, gate: Gate) -> Option<Assignment> {
        if !gate.allow_any {
            return None;
        }

        let refine_running = self
            .running
            .iter()
            .filter(|r| r.stage == Stage::Refine)
            .count();
        let index_running = self
            .running
            .iter()
            .filter(|r| r.stage == Stage::Index)
            .count();

        let mut best: Option<usize> = None;
        for (i, task) in self.pending.iter().enumerate() {
            if task.not_before_ms > now_ms || !gate.permits(task.stage) {
                continue;
            }
            let has_capacity = match task.stage {
                Stage::Refine => refine_running < self.policy.max_refine,
                Stage::Index => index_running < self.policy.max_index,
            };
            if !has_capacity {
                continue;
            }
            best = match best {
                None => Some(i),
                Some(b) => {
                    let cur = &self.pending[b];
                    // 先比优先级，再比入队序号（FIFO）
                    if (task.priority.rank(), task.seq) < (cur.priority.rank(), cur.seq) {
                        Some(i)
                    } else {
                        Some(b)
                    }
                }
            };
        }

        let task = self.pending.remove(best?);
        self.running.push(InFlight {
            book: task.book,
            stage: task.stage,
            priority: task.priority,
            attempts: task.attempts,
            started_ms: now_ms,
        });
        Some(Assignment {
            book: task.book,
            stage: task.stage,
        })
    }

    /// 反复取任务直到没有可派的（受闸门与并发上限约束）。
    ///
    /// 返回的每个任务都必须由调用方以 [`Scheduler::complete`] /
    /// [`Scheduler::fail`] / [`Scheduler::defer`] 三者之一收尾。
    pub fn tick(&mut self, now_ms: u64, gate: Gate) -> Vec<Assignment> {
        let mut out = Vec::new();
        while let Some(a) = self.next(now_ms, gate) {
            out.push(a);
        }
        out
    }

    /// 任务成功完成。
    pub fn complete(&mut self, book: BookId) -> bool {
        self.remove_running(book).is_some()
    }

    /// 任务**没干完但不是失败**（时间片用完、App 转入后台）：
    /// 放回队列，**不计入失败次数、不触发退避**，可立刻被重新调度。
    pub fn defer(&mut self, book: BookId, now_ms: u64) -> bool {
        let Some(done) = self.remove_running(book) else {
            return false;
        };
        self.pending.push(Task {
            book: done.book,
            stage: done.stage,
            priority: done.priority,
            attempts: done.attempts,
            not_before_ms: now_ms,
            seq: self.next_seq,
        });
        self.next_seq = self.next_seq.saturating_add(1);
        true
    }

    /// 任务失败：累计一次尝试，未用尽则按指数退避重新排队。
    pub fn fail(&mut self, book: BookId, now_ms: u64) -> FailOutcome {
        let Some(done) = self.remove_running(book) else {
            return FailOutcome::NotRunning;
        };
        let attempts = done.attempts.saturating_add(1);
        if attempts >= self.policy.max_attempts {
            self.exhausted.push(Exhausted {
                book: done.book,
                stage: done.stage,
                attempts,
            });
            return FailOutcome::Exhausted;
        }
        let not_before_ms = now_ms.saturating_add(self.backoff_ms(attempts));
        self.pending.push(Task {
            book: done.book,
            stage: done.stage,
            priority: done.priority,
            attempts,
            not_before_ms,
            seq: self.next_seq,
        });
        self.next_seq = self.next_seq.saturating_add(1);
        FailOutcome::RetryScheduled {
            attempts,
            not_before_ms,
        }
    }

    /// 第 `attempts` 次失败后的退避时长：`base * 2^(attempts-1)`，封顶 `backoff_max_ms`。
    #[must_use]
    pub fn backoff_ms(&self, attempts: u16) -> u64 {
        // 移位上限 16，避免 1u64 << 63 之类的溢出；反正会被 backoff_max_ms 截断
        let shift = u32::from(attempts.saturating_sub(1)).min(16);
        let raw = self.policy.backoff_base_ms.saturating_mul(1u64 << shift);
        raw.min(self.policy.backoff_max_ms)
    }

    /// 提升优先级（用户打开了这本书，要立刻抢资源）。
    ///
    /// 只会往更紧急的方向调；已经在运行的任务也会同步更新记录。返回是否发生变化。
    pub fn promote(&mut self, book: BookId, priority: Priority) -> bool {
        let mut changed = false;
        for task in &mut self.pending {
            if task.book == book && priority.is_more_urgent_than(task.priority) {
                task.priority = priority;
                changed = true;
            }
        }
        for run in &mut self.running {
            if run.book == book && priority.is_more_urgent_than(run.priority) {
                run.priority = priority;
                changed = true;
            }
        }
        changed
    }

    /// 取消一本书（书被删除、或用户选择放弃）。
    ///
    /// 排队中与运行中的都会被移除，同时腾出并发名额。返回是否找到过该书。
    pub fn cancel(&mut self, book: BookId) -> bool {
        let before = self.pending.len() + self.running.len();
        self.pending.retain(|t| t.book != book);
        self.running.retain(|r| r.book != book);
        self.exhausted.retain(|e| e.book != book);
        self.pending.len() + self.running.len() < before
    }

    /// 手动重试一个已耗尽的任务：**清零失败次数**，以 `Refine` 重新排队。
    ///
    /// 从②重新开始是有意的：②便宜且幂等，而从失败的③直接续跑需要更细的检查点。
    pub fn revive(&mut self, book: BookId, priority: Priority) -> bool {
        let Some(pos) = self.exhausted.iter().position(|e| e.book == book) else {
            return false;
        };
        self.exhausted.remove(pos);
        if self.contains(book) {
            return false;
        }
        self.pending.push(Task {
            book,
            stage: Stage::Refine,
            priority,
            attempts: 0,
            not_before_ms: 0,
            seq: self.next_seq,
        });
        self.next_seq = self.next_seq.saturating_add(1);
        true
    }

    /// 当前正在运行的任务（用于"正在精修 N 本"这类 UI）。
    #[must_use]
    pub fn in_flight(&self) -> &[InFlight] {
        &self.running
    }

    /// 排队中的任务快照。
    #[must_use]
    pub fn pending(&self) -> &[Task] {
        &self.pending
    }

    /// 重试耗尽的清单。
    #[must_use]
    pub fn exhausted(&self) -> &[Exhausted] {
        &self.exhausted
    }

    /// 统计快照。
    #[must_use]
    pub fn stats(&self) -> Stats {
        Stats {
            pending_refine: self
                .pending
                .iter()
                .filter(|t| t.stage == Stage::Refine)
                .count(),
            pending_index: self
                .pending
                .iter()
                .filter(|t| t.stage == Stage::Index)
                .count(),
            in_flight: self.running.len(),
            exhausted: self.exhausted.len(),
        }
    }

    /// 队列是否已空（可以通知 UI"全部处理完"）。
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.running.is_empty()
    }

    fn remove_running(&mut self, book: BookId) -> Option<InFlight> {
        let pos = self.running.iter().position(|r| r.book == book)?;
        Some(self.running.remove(pos))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn sched() -> Scheduler {
        Scheduler::new(Policy::default()).unwrap()
    }

    /// 依次取任务并**立刻 complete**（否则占着的并发名额不会释放，
    /// 下一次 next() 会返回 None）。用于断言派活顺序。
    fn drain_order(s: &mut Scheduler, n: usize) -> Vec<BookId> {
        let mut out = Vec::new();
        for step in 0..n {
            let Some(assignment) = s.next(step as u64, Gate::foreground()) else {
                break;
            };
            out.push(assignment.book);
            s.complete(assignment.book);
        }
        out
    }

    /// 阶段②一次只派一本，便于断言顺序。
    fn sched_serial() -> Scheduler {
        Scheduler::new(Policy {
            max_refine: 1,
            max_index: 1,
            ..Policy::default()
        })
        .unwrap()
    }

    // ---------- 状态 → 阶段 ----------

    #[test]
    fn stage_derivation_matches_pipeline() {
        assert_eq!(
            Scheduler::stage_for(ImportState::Registered),
            Some(Stage::Refine)
        );
        // 中途被杀：重新做②，②是幂等的
        assert_eq!(
            Scheduler::stage_for(ImportState::Refining),
            Some(Stage::Refine)
        );
        assert_eq!(
            Scheduler::stage_for(ImportState::Indexing),
            Some(Stage::Index)
        );
        assert_eq!(Scheduler::stage_for(ImportState::Ready), None);
        // failed 不自动重排：必须显式 revive，否则重启会无限重试
        assert_eq!(Scheduler::stage_for(ImportState::Failed), None);
    }

    #[test]
    fn enqueue_reports_outcome() {
        let mut s = sched();
        let a = BookId::new();
        assert_eq!(
            s.enqueue(a, ImportState::Registered, Priority::Normal),
            EnqueueOutcome::Queued(Stage::Refine)
        );
        let b = BookId::new();
        assert_eq!(
            s.enqueue(b, ImportState::Indexing, Priority::Normal),
            EnqueueOutcome::Queued(Stage::Index)
        );
        let c = BookId::new();
        assert_eq!(
            s.enqueue(c, ImportState::Ready, Priority::Normal),
            EnqueueOutcome::NotNeeded
        );
        let d = BookId::new();
        assert_eq!(
            s.enqueue(d, ImportState::Failed, Priority::Normal),
            EnqueueOutcome::NotNeeded
        );
    }

    #[test]
    fn enqueue_is_idempotent_even_while_running() {
        let mut s = sched_serial();
        let a = BookId::new();
        assert_eq!(
            s.enqueue(a, ImportState::Registered, Priority::Normal),
            EnqueueOutcome::Queued(Stage::Refine)
        );
        // 排队中重复登记：不产生第二条
        assert_eq!(
            s.enqueue(a, ImportState::Registered, Priority::Normal),
            EnqueueOutcome::AlreadyPresent
        );
        assert_eq!(s.pending().len(), 1);

        // 取走运行后仍然不能重复登记
        assert_eq!(s.next(0, Gate::foreground()).map(|a| a.book), Some(a));
        assert_eq!(
            s.enqueue(a, ImportState::Refining, Priority::Urgent),
            EnqueueOutcome::AlreadyPresent
        );
        assert_eq!(s.pending().len(), 0);
        assert_eq!(s.in_flight().len(), 1);
    }

    // ---------- 优先级与 FIFO ----------

    #[test]
    fn priority_beats_insertion_order() {
        let mut s = sched_serial();
        let normal = BookId::new();
        let background = BookId::new();
        let urgent = BookId::new();
        s.enqueue(normal, ImportState::Registered, Priority::Normal);
        s.enqueue(background, ImportState::Registered, Priority::Background);
        // 最后进来的最紧急，应当第一个被派
        s.enqueue(urgent, ImportState::Registered, Priority::Urgent);

        assert_eq!(drain_order(&mut s, 3), vec![urgent, normal, background]);
    }

    #[test]
    fn same_priority_is_fifo() {
        let mut s = sched_serial();
        let a = BookId::new();
        let b = BookId::new();
        let c = BookId::new();
        for book in [a, b, c] {
            s.enqueue(book, ImportState::Registered, Priority::Normal);
        }
        assert_eq!(drain_order(&mut s, 3), vec![a, b, c]);
    }

    #[test]
    fn priority_rank_is_smaller_is_urgent() {
        assert_eq!(Priority::Urgent.rank(), 0);
        assert!(Priority::Urgent.is_more_urgent_than(Priority::Normal));
        assert!(Priority::Normal.is_more_urgent_than(Priority::Background));
        assert!(!Priority::Background.is_more_urgent_than(Priority::Urgent));
        // 与 derive(Ord) 的顺序一致
        assert!(Priority::Urgent < Priority::Normal);
        assert_eq!(Priority::default(), Priority::Normal);
    }

    // ---------- 并发上限 ----------

    #[test]
    fn refine_respects_concurrency_limit() {
        let mut s = sched(); // max_refine = 2
        for _ in 0..3 {
            s.enqueue(BookId::new(), ImportState::Registered, Priority::Normal);
        }
        let batch = s.tick(0, Gate::foreground());
        assert_eq!(batch.len(), 2, "②的并发上限是 2");
        assert_eq!(s.stats().in_flight, 2);
        assert_eq!(s.stats().pending_refine, 1);
        // 名额占满后不再派活
        assert!(s.tick(0, Gate::foreground()).is_empty());
    }

    #[test]
    fn index_is_serial_by_default() {
        let mut s = sched();
        for _ in 0..3 {
            s.enqueue(BookId::new(), ImportState::Indexing, Priority::Normal);
        }
        let batch = s.tick(0, Gate::foreground());
        assert_eq!(batch.len(), 1, "③默认串行，避免抢走主线程");
        assert_eq!(batch[0].stage, Stage::Index);
        assert_eq!(s.stats().pending_index, 2);
    }

    #[test]
    fn mixed_stages_fill_both_pools() {
        let mut s = sched();
        for _ in 0..3 {
            s.enqueue(BookId::new(), ImportState::Registered, Priority::Normal);
        }
        for _ in 0..2 {
            s.enqueue(BookId::new(), ImportState::Indexing, Priority::Normal);
        }
        let batch = s.tick(0, Gate::foreground());
        // 2 个② + 1 个③
        assert_eq!(batch.len(), 3);
        assert_eq!(batch.iter().filter(|a| a.stage == Stage::Refine).count(), 2);
        assert_eq!(batch.iter().filter(|a| a.stage == Stage::Index).count(), 1);
    }

    // ---------- 闸门 ----------

    #[test]
    fn suspended_gate_blocks_everything() {
        let mut s = sched();
        s.enqueue(BookId::new(), ImportState::Registered, Priority::Urgent);
        s.enqueue(BookId::new(), ImportState::Indexing, Priority::Urgent);
        assert!(s.tick(0, Gate::suspended()).is_empty());
        assert_eq!(s.stats().in_flight, 0);
        // 任务没丢
        assert_eq!(s.stats().pending_refine, 1);
        assert_eq!(s.stats().pending_index, 1);
    }

    #[test]
    fn throttled_gate_allows_only_light_stage() {
        let mut s = sched();
        let book = BookId::new();
        s.enqueue(book, ImportState::Registered, Priority::Normal);
        let idx = BookId::new();
        s.enqueue(idx, ImportState::Indexing, Priority::Normal);

        let batch = s.tick(0, Gate::throttled());
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].book, book);
        assert_eq!(batch[0].stage, Stage::Refine);
        // ③被闸门挡住，仍在排队
        assert_eq!(s.stats().pending_index, 1);
    }

    #[test]
    fn gate_permits_matrix() {
        assert!(Gate::foreground().permits(Stage::Refine));
        assert!(Gate::foreground().permits(Stage::Index));
        assert!(Gate::throttled().permits(Stage::Refine));
        assert!(!Gate::throttled().permits(Stage::Index));
        assert!(!Gate::suspended().permits(Stage::Refine));
        assert!(!Gate::suspended().permits(Stage::Index));
        assert!(!Stage::Refine.is_heavy());
        assert!(Stage::Index.is_heavy());
    }

    // ---------- 失败与退避 ----------

    #[test]
    fn failure_schedules_backoff_retry() {
        let mut s = sched_serial();
        let book = BookId::new();
        s.enqueue(book, ImportState::Registered, Priority::Normal);

        assert_eq!(
            s.next(1_000, Gate::foreground()).map(|a| a.book),
            Some(book)
        );
        let outcome = s.fail(book, 1_000);
        assert_eq!(
            outcome,
            FailOutcome::RetryScheduled {
                attempts: 1,
                not_before_ms: 3_000,
            }
        );

        // 退避窗口内不派活
        assert!(s.next(2_999, Gate::foreground()).is_none());
        // 到点立刻可重试
        assert_eq!(
            s.next(3_000, Gate::foreground()).map(|a| a.book),
            Some(book)
        );
    }

    #[test]
    fn fail_on_unknown_book_is_reported() {
        let mut s = sched();
        assert_eq!(s.fail(BookId::new(), 0), FailOutcome::NotRunning);
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        let s = sched(); // base 2000, max 60000
        assert_eq!(s.backoff_ms(1), 2_000);
        assert_eq!(s.backoff_ms(2), 4_000);
        assert_eq!(s.backoff_ms(3), 8_000);
        assert_eq!(s.backoff_ms(4), 16_000);
        assert_eq!(s.backoff_ms(5), 32_000);
        // 64_000 被截到 60_000
        assert_eq!(s.backoff_ms(6), 60_000);
        assert_eq!(s.backoff_ms(30), 60_000);
        // attempts = 0 不能溢出/下溢
        assert_eq!(s.backoff_ms(0), 2_000);
    }

    #[test]
    fn attempts_are_capped_and_then_exhausted() {
        let mut s = sched_serial(); // max_attempts = 3

        let book = BookId::new();
        s.enqueue(book, ImportState::Registered, Priority::Normal);

        // 第 1 次失败 → 重试
        s.next(0, Gate::foreground());
        assert!(matches!(
            s.fail(book, 0),
            FailOutcome::RetryScheduled { attempts: 1, .. }
        ));
        // 第 2 次失败 → 重试
        s.next(2_000, Gate::foreground());
        assert!(matches!(
            s.fail(book, 2_000),
            FailOutcome::RetryScheduled { attempts: 2, .. }
        ));
        // 第 3 次失败 → 放弃
        s.next(6_000, Gate::foreground());
        assert_eq!(s.fail(book, 6_000), FailOutcome::Exhausted);

        assert_eq!(s.exhausted().len(), 1);
        assert_eq!(s.exhausted()[0].book, book);
        assert_eq!(s.exhausted()[0].attempts, 3);
        assert_eq!(s.exhausted()[0].stage, Stage::Refine);
        // 不再被调度
        assert!(s.next(1_000_000, Gate::foreground()).is_none());
        assert!(!s.contains(book));
    }

    #[test]
    fn resume_preserves_attempt_count() {
        let mut s = sched_serial();
        let book = BookId::new();
        // 崩溃前已经失败过 2 次
        assert_eq!(
            s.resume(book, ImportState::Refining, Priority::Normal, 2),
            EnqueueOutcome::Queued(Stage::Refine)
        );
        assert_eq!(s.pending()[0].attempts, 2);

        s.next(0, Gate::foreground());
        // 第 3 次失败就直接耗尽，而不是从头开始数
        assert_eq!(s.fail(book, 0), FailOutcome::Exhausted);
    }

    // ---------- 收尾三条路径 ----------

    #[test]
    fn complete_drains_to_idle() {
        let mut s = sched();
        let book = BookId::new();
        s.enqueue(book, ImportState::Registered, Priority::Normal);
        s.next(0, Gate::foreground());
        assert_eq!(s.stats().in_flight, 1);

        assert!(s.complete(book));
        assert!(s.is_idle());
        assert_eq!(s.stats(), Stats::default());
        // 重复 complete 无效
        assert!(!s.complete(book));
    }

    #[test]
    fn defer_requeues_without_counting_an_attempt() {
        let mut s = sched_serial();
        let book = BookId::new();
        s.enqueue(book, ImportState::Registered, Priority::Normal);

        s.next(0, Gate::foreground());
        assert!(s.defer(book, 500));
        assert_eq!(s.stats().in_flight, 0);
        assert_eq!(s.pending().len(), 1);
        // 不计失败、不触发退避：立刻能再派
        assert_eq!(s.pending()[0].attempts, 0);
        assert_eq!(s.pending()[0].not_before_ms, 500);
        assert_eq!(s.next(500, Gate::foreground()).map(|a| a.book), Some(book));
    }

    #[test]
    fn defer_on_unknown_book_is_noop() {
        let mut s = sched();
        assert!(!s.defer(BookId::new(), 0));
    }

    // ---------- 取消 / 提权 / 复活 ----------

    #[test]
    fn cancel_frees_a_concurrency_slot() {
        let mut s = sched_serial();
        let a = BookId::new();
        let b = BookId::new();
        s.enqueue(a, ImportState::Registered, Priority::Normal);
        s.enqueue(b, ImportState::Registered, Priority::Normal);

        assert_eq!(s.next(0, Gate::foreground()).map(|x| x.book), Some(a));
        // 此时②的名额被 a 占着
        assert!(s.next(0, Gate::foreground()).is_none());

        assert!(s.cancel(a));
        // 名额腾出来，b 立刻能跑
        assert_eq!(s.next(0, Gate::foreground()).map(|x| x.book), Some(b));
        assert!(!s.contains(a));
    }

    #[test]
    fn cancel_unknown_book_reports_false() {
        let mut s = sched();
        s.enqueue(BookId::new(), ImportState::Registered, Priority::Normal);
        assert!(!s.cancel(BookId::new()));
        assert_eq!(s.pending().len(), 1);
    }

    #[test]
    fn promote_makes_a_book_jump_the_queue() {
        let mut s = sched_serial();
        let a = BookId::new();
        let b = BookId::new();
        let c = BookId::new();
        for book in [a, b, c] {
            s.enqueue(book, ImportState::Registered, Priority::Normal);
        }
        assert_eq!(s.next(0, Gate::foreground()).map(|x| x.book), Some(a));

        // 用户打开了 c
        assert!(s.promote(c, Priority::Urgent));
        s.complete(a);

        // c 越过 b
        assert_eq!(s.next(1, Gate::foreground()).map(|x| x.book), Some(c));
        // 重复提权不报"变化"
        assert!(!s.promote(c, Priority::Urgent));
        // 降级不算变化
        assert!(!s.promote(b, Priority::Background));
    }

    #[test]
    fn revive_requeues_an_exhausted_book() {
        let mut s = sched_serial();
        let book = BookId::new();
        s.enqueue(book, ImportState::Registered, Priority::Normal);
        for at in [0u64, 2_000, 6_000] {
            s.next(at, Gate::foreground());
            s.fail(book, at);
        }
        assert_eq!(s.exhausted().len(), 1);

        // 用户点了"重试"
        assert!(s.revive(book, Priority::Normal));
        assert!(s.exhausted().is_empty());
        assert_eq!(s.pending().len(), 1);
        assert_eq!(s.pending()[0].stage, Stage::Refine);
        assert_eq!(s.pending()[0].attempts, 0, "手动重试应清零失败次数");

        // 已在队列里时不能重复复活
        assert!(!s.revive(book, Priority::Normal));
        // 没耗尽过的书也不能复活
        assert!(!s.revive(BookId::new(), Priority::Normal));
    }

    #[test]
    fn revive_does_not_duplicate_a_queued_book() {
        let mut s = sched_serial();
        let book = BookId::new();
        s.enqueue(book, ImportState::Registered, Priority::Normal);
        for at in [0u64, 2_000, 6_000] {
            s.next(at, Gate::foreground());
            s.fail(book, at);
        }
        // 手工把它塞回队列，再尝试复活
        s.enqueue(book, ImportState::Registered, Priority::Normal);
        assert!(!s.revive(book, Priority::Normal));
        assert_eq!(s.pending().len(), 1);
    }

    // ---------- 统计与策略校验 ----------

    #[test]
    fn stats_track_the_queue() {
        let mut s = sched();
        s.enqueue(BookId::new(), ImportState::Registered, Priority::Normal);
        s.enqueue(BookId::new(), ImportState::Registered, Priority::Normal);
        s.enqueue(BookId::new(), ImportState::Indexing, Priority::Normal);

        let stats = s.stats();
        assert_eq!(stats.pending_refine, 2);
        assert_eq!(stats.pending_index, 1);
        assert_eq!(stats.in_flight, 0);
        assert_eq!(stats.exhausted, 0);

        s.next(0, Gate::foreground());
        let stats = s.stats();
        assert_eq!(stats.in_flight, 1);
        assert_eq!(stats.pending_refine, 1);
    }

    #[test]
    fn empty_scheduler_is_idle() {
        let s = sched();
        assert!(s.is_idle());
        assert_eq!(s.stats(), Stats::default());
        assert!(s.in_flight().is_empty());
        assert!(s.pending().is_empty());
        assert!(s.exhausted().is_empty());
    }

    #[test]
    fn policy_validation_rejects_nonsense() {
        assert_eq!(
            Scheduler::new(Policy {
                max_refine: 0,
                max_index: 0,
                ..Policy::default()
            })
            .err(),
            Some(PipelineError::NoConcurrency)
        );
        assert_eq!(
            Scheduler::new(Policy {
                max_attempts: 0,
                ..Policy::default()
            })
            .err(),
            Some(PipelineError::ZeroAttempts)
        );
        assert_eq!(
            Scheduler::new(Policy {
                backoff_base_ms: 10_000,
                backoff_max_ms: 1_000,
                ..Policy::default()
            })
            .err(),
            Some(PipelineError::BackoffInverted)
        );
        assert!(Policy::default().validate().is_ok());
        // 只开一侧并发是合法的：低电量时只想做②
        assert!(
            Policy {
                max_index: 0,
                ..Policy::default()
            }
            .validate()
            .is_ok()
        );
    }

    // ---------- 端到端：一次完整导入 ----------

    #[test]
    fn full_pipeline_runs_to_ready() {
        let mut s = sched();
        let books: Vec<BookId> = (0..4).map(|_| BookId::new()).collect();
        for &book in &books {
            assert_eq!(
                s.enqueue(book, ImportState::Registered, Priority::Normal),
                EnqueueOutcome::Queued(Stage::Refine)
            );
        }

        // 每一轮：取活 → 收尾 → ②成功的转③，直到全部处理完。
        // 注意必须 complete 掉每一个指派，否则并发名额被永久占住，调度器会假死。
        let mut most_index_in_one_tick = 0;
        let mut ticks: u64 = 0;
        while !s.is_idle() {
            ticks += 1;
            assert!(ticks < 100, "调度器应能收敛，不该无限循环");
            let batch = s.tick(ticks, Gate::foreground());
            let index_now = batch.iter().filter(|a| a.stage == Stage::Index).count();
            most_index_in_one_tick = most_index_in_one_tick.max(index_now);
            for assignment in batch {
                s.complete(assignment.book);
                if assignment.stage == Stage::Refine {
                    // ②做完就排③；状态取自库里的记录
                    assert_eq!(
                        s.enqueue(assignment.book, ImportState::Indexing, Priority::Normal),
                        EnqueueOutcome::Queued(Stage::Index)
                    );
                }
            }
        }

        assert_eq!(most_index_in_one_tick, 1, "③在任何一轮里都只派一个");
        assert!(s.exhausted().is_empty());
        assert_eq!(s.stats(), Stats::default());
    }
}
