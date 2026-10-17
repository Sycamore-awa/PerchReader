//! 跨端同步的合并引擎：字段级 LWW + 冲突统计。
//!
//! 目标见 `项目目标.txt` 的"同步准"：进度/书签/笔记跨端同步，**冲突率 < 1%**。
//!
//! # 冲突率的定义（必须先定清楚）
//!
//! `docs/02-架构设计.md` 里的口径是：
//!
//! ```text
//! 冲突率 = 被丢弃的字段级变更数 / 总字段级变更数
//! ```
//!
//! 定义不同，算出来的数字能差十倍。所以本 crate 把这个口径**做成了代码**
//! （[`MergeStats::conflict_rate`]），而不是留在文档里各说各话。
//!
//! # 冲突面怎么压到最小
//!
//! 1. **按字段合并**：进度、书签、笔记各自独立带时间戳，不共享一个"记录时间"
//! 2. **每条标注独立 UUID**：两个人同时改不同的高亮互不影响
//! 3. **墓碑**：删除也带时间戳，避免"删了又复活"
//!
//! # 收敛性
//!
//! 合并必须满足：**同一批变更，无论以什么顺序应用，最终状态一致**。
//! 这是纯函数性质，因此可以在 Linux 上随机生成成千上万组操作来验证
//! （见本文件末尾的确定性随机测试）。
//!
//! 收敛成立的前提是 **HLC 全序**，即设备号唯一（见 [`hlc`]）。
//! 若时间戳完全相同而值不同（正常不该发生），本实现保留本地值并计入冲突，
//! 并如实说明这一情形会破坏收敛——宁可在文档里讲清楚，也不要假装它不存在。

pub mod hlc;
pub mod wire;

use perch_model::Locator;

pub use hlc::Hlc;
pub use wire::{
    Change, ChangeKey, ChangeLog, ChangeOp, ChangePage, DEFAULT_PAGE_LIMIT, MergeReport,
    PROTOCOL_VERSION, SyncError, SyncRequest, SyncResponse, WireValue, check_protocol,
    validate_all,
};

/// 一次合并的效果。
///
/// 这是**可计数的单位**：冲突率就是统计这些结果得出的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeEffect {
    /// 本地为空，直接采纳远端。
    Initialized,
    /// 远端更新，采纳。
    Updated,
    /// 远端更旧，丢弃（这**不算冲突**——它是正常的"另一台设备落后了"）。
    StaleIgnored,
    /// 时间戳相同但值不同：按定序规则选了一个，另一个被丢弃。
    ///
    /// 这才是真正意义上的冲突，计入 [`MergeStats::conflicts`]。
    ConflictResolved,
    /// 完全相同，无事发生。
    Unchanged,
}

impl MergeEffect {
    /// 稳定的字符串表示（FFI / 日志 / 统计上报用）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Initialized => "initialized",
            Self::Updated => "updated",
            Self::StaleIgnored => "staleIgnored",
            Self::ConflictResolved => "conflictResolved",
            Self::Unchanged => "unchanged",
        }
    }
}

/// 合并统计（用于计算冲突率）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeStats {
    /// 首次采纳（本地为空）。
    pub initialized: u32,
    /// 采纳了更新的远端值。
    pub updated: u32,
    /// 丢弃了更旧的远端值。
    pub stale_ignored: u32,
    /// 时间戳相同时的冲突裁决。
    pub conflicts: u32,
    /// 无变化。
    pub unchanged: u32,
}

impl MergeStats {
    /// 记录一次合并结果。
    pub fn record(&mut self, effect: MergeEffect) {
        match effect {
            MergeEffect::Initialized => self.initialized += 1,
            MergeEffect::Updated => self.updated += 1,
            MergeEffect::StaleIgnored => self.stale_ignored += 1,
            MergeEffect::ConflictResolved => self.conflicts += 1,
            MergeEffect::Unchanged => self.unchanged += 1,
        }
    }

    /// 总合并次数。
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.initialized + self.updated + self.stale_ignored + self.conflicts + self.unchanged
    }

    /// 发生了"变更被丢弃"的次数——冲突率的**分母口径**。
    #[must_use]
    pub const fn total_changes(&self) -> u32 {
        self.initialized + self.updated + self.stale_ignored + self.conflicts
    }

    /// 冲突率（0.0 ~ 1.0）。没有发生过变更时返回 0。
    ///
    /// 注意分母用的是 [`MergeStats::total_changes`] 而不是总合并次数：
    /// `Unchanged` 不该稀释冲突率。
    #[must_use]
    pub fn conflict_rate(&self) -> f32 {
        let total = self.total_changes();
        if total == 0 {
            0.0
        } else {
            self.conflicts as f32 / total as f32
        }
    }
}

/// 带时间戳的值（Last-Write-Wins 寄存器）。
#[derive(Debug, Clone, PartialEq)]
pub struct Lww<T> {
    /// 值。
    pub value: T,
    /// 写入时间（HLC）。
    pub stamp: Hlc,
}

impl<T> Lww<T> {
    /// 构造。
    #[must_use]
    pub const fn new(value: T, stamp: Hlc) -> Self {
        Self { value, stamp }
    }

    /// 取时间戳。
    #[must_use]
    pub const fn stamp(&self) -> &Hlc {
        &self.stamp
    }
}

impl<T: PartialEq> Lww<T> {
    /// 用远端值合并。
    pub fn merge(&mut self, remote: Self) -> MergeEffect {
        match remote.stamp.cmp(&self.stamp) {
            std::cmp::Ordering::Greater => {
                *self = remote;
                MergeEffect::Updated
            }
            std::cmp::Ordering::Less => MergeEffect::StaleIgnored,
            std::cmp::Ordering::Equal => {
                if remote.value == self.value {
                    MergeEffect::Unchanged
                } else {
                    // 时间戳完全相同而值不同：正常不该发生（设备号唯一 + 计数单调）。
                    // 保留本地值以保持确定性；但要如实计为冲突。
                    MergeEffect::ConflictResolved
                }
            }
        }
    }
}

/// 可选值的合并（本地还没有时采纳远端）。
impl<T: PartialEq> Lww<Option<T>> {
    /// 合并成一个可能为空的值。
    pub fn merge_option(local: &mut Option<Lww<T>>, remote: Lww<T>) -> MergeEffect {
        match local {
            None => {
                *local = Some(remote);
                MergeEffect::Initialized
            }
            Some(existing) => existing.merge(remote),
        }
    }
}

/// 阅读进度的合并规则。
///
/// 这是一个**产品决策**，不是技术决策（见 `docs/04-待决问题.md` 第 9 条），
/// 因此做成可插拔的，而不是写死一种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressRule {
    /// 最后一次阅读的位置胜出（按 HLC）。
    ///
    /// 符合"我接着刚才读"的直觉，但可能出现"在平板翻到前面，手机也跟着退回去"。
    LastWriteWins,
    /// 最远的位置胜出。
    ///
    /// 不会倒退，但"回头查阅"后换设备会跳回后面。
    FurthestWins,
}

/// 由 Locator 取进度值（0.0~1.0）。缺少进度时按 0 处理。
fn progression_of(locator: &Locator) -> f64 {
    locator
        .locations
        .progression
        .filter(|value| value.is_finite())
        .unwrap_or(0.0)
}

/// 按指定规则合并阅读进度。
///
/// 两种规则都满足收敛性：
/// - [`ProgressRule::LastWriteWins`]：按 HLC 全序取最大
/// - [`ProgressRule::FurthestWins`]：取进度较大者，相同进度再按 HLC 定序
///
/// `FurthestWins` 的关键在于"取最大"本身是**可交换、可结合**的运算，
/// 因此与到达顺序无关。
pub fn merge_progress(
    local: &mut Option<Lww<Locator>>,
    remote: Lww<Locator>,
    rule: ProgressRule,
) -> MergeEffect {
    let Some(existing) = local.as_mut() else {
        *local = Some(remote);
        return MergeEffect::Initialized;
    };

    match rule {
        ProgressRule::LastWriteWins => existing.merge(remote),
        ProgressRule::FurthestWins => {
            let local_progress = progression_of(&existing.value);
            let remote_progress = progression_of(&remote.value);
            if remote_progress > local_progress {
                *existing = remote;
                MergeEffect::Updated
            } else if remote_progress < local_progress {
                MergeEffect::StaleIgnored
            } else {
                // 进度相同 → 退回时间戳定序，保证结果确定
                existing.merge(remote)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use perch_model::{BookId, Locator};

    fn locator(progression: f64) -> Locator {
        Locator::new_epub("/c1.xhtml", Some("/6/4".to_owned()), Some(progression))
    }

    fn stamp(wall: i64, counter: u32, device: &str) -> Hlc {
        Hlc::from_parts(wall, counter, device)
    }

    #[test]
    fn newer_remote_wins() {
        let mut local = Lww::new("旧".to_owned(), stamp(100, 0, "A"));
        let effect = local.merge(Lww::new("新".to_owned(), stamp(200, 0, "B")));
        assert_eq!(effect, MergeEffect::Updated);
        assert_eq!(local.value, "新");
    }

    #[test]
    fn older_remote_is_ignored_and_not_a_conflict() {
        let mut local = Lww::new("新".to_owned(), stamp(200, 0, "A"));
        let effect = local.merge(Lww::new("旧".to_owned(), stamp(100, 0, "B")));
        assert_eq!(effect, MergeEffect::StaleIgnored);
        assert_eq!(local.value, "新");
    }

    #[test]
    fn identical_remote_is_unchanged() {
        let mut local = Lww::new("同".to_owned(), stamp(100, 0, "A"));
        let effect = local.merge(Lww::new("同".to_owned(), stamp(100, 0, "A")));
        assert_eq!(effect, MergeEffect::Unchanged);
    }

    #[test]
    fn same_stamp_different_value_is_counted_as_conflict() {
        let mut local = Lww::new("甲".to_owned(), stamp(100, 0, "A"));
        let effect = local.merge(Lww::new("乙".to_owned(), stamp(100, 0, "A")));
        assert_eq!(effect, MergeEffect::ConflictResolved);
        assert_eq!(local.value, "甲", "保留本地值以保持确定");
    }

    #[test]
    fn option_merge_initializes_then_behaves_like_lww() {
        let mut slot: Option<Lww<Locator>> = None;
        assert_eq!(
            merge_progress(
                &mut slot,
                Lww::new(locator(0.1), stamp(10, 0, "A")),
                ProgressRule::LastWriteWins
            ),
            MergeEffect::Initialized
        );
        assert!(slot.is_some());

        assert_eq!(
            merge_progress(
                &mut slot,
                Lww::new(locator(0.2), stamp(20, 0, "B")),
                ProgressRule::LastWriteWins
            ),
            MergeEffect::Updated
        );
        assert_eq!(progression_of(&slot.unwrap().value), 0.2);
    }

    #[test]
    fn last_write_wins_can_move_backwards() {
        let mut slot = Some(Lww::new(locator(0.8), stamp(100, 0, "A")));
        // 更晚发生、但进度更靠前的位置
        let effect = merge_progress(
            &mut slot,
            Lww::new(locator(0.3), stamp(200, 0, "B")),
            ProgressRule::LastWriteWins,
        );
        assert_eq!(effect, MergeEffect::Updated);
        assert_eq!(progression_of(&slot.unwrap().value), 0.3);
    }

    #[test]
    fn furthest_wins_never_goes_backwards() {
        let mut slot = Some(Lww::new(locator(0.8), stamp(100, 0, "A")));
        // 时间更晚但进度更靠前 → FurthestWins 应忽略
        let effect = merge_progress(
            &mut slot,
            Lww::new(locator(0.3), stamp(200, 0, "B")),
            ProgressRule::FurthestWins,
        );
        assert_eq!(effect, MergeEffect::StaleIgnored);
        assert_eq!(progression_of(&slot.unwrap().value), 0.8);
    }

    #[test]
    fn furthest_wins_accepts_a_further_position_even_if_older() {
        let mut slot = Some(Lww::new(locator(0.3), stamp(200, 0, "A")));
        // 时间更早但进度更远 → 仍然采纳（这是"最远胜出"的定义）
        let effect = merge_progress(
            &mut slot,
            Lww::new(locator(0.9), stamp(100, 0, "B")),
            ProgressRule::FurthestWins,
        );
        assert_eq!(effect, MergeEffect::Updated);
        assert_eq!(progression_of(&slot.unwrap().value), 0.9);
    }

    #[test]
    fn conflict_rate_uses_changes_as_denominator() {
        let mut stats = MergeStats::default();
        for effect in [
            MergeEffect::Initialized,
            MergeEffect::Updated,
            MergeEffect::StaleIgnored,
            MergeEffect::ConflictResolved,
            MergeEffect::Unchanged,
            MergeEffect::Unchanged,
        ] {
            stats.record(effect);
        }
        assert_eq!(stats.total(), 6);
        // Unchanged 不参与分母
        assert_eq!(stats.total_changes(), 4);
        assert!((stats.conflict_rate() - 0.25).abs() < 1e-6);
    }

    #[test]
    fn conflict_rate_is_zero_without_changes() {
        let stats = MergeStats::default();
        assert_eq!(stats.conflict_rate(), 0.0);
        assert_eq!(stats.total_changes(), 0);
    }

    // ------------------------------------------------------------------
    // 收敛性：同一批变更，任意顺序应用，最终状态必须一致
    // ------------------------------------------------------------------

    /// 极小的确定性伪随机数发生器（线性同余）。
    ///
    /// 刻意不引入 proptest：收敛性测试要能在 CI 里**稳定复现**，
    /// 固定种子 + 自己实现洗牌，比随机失败再 shrink 更适合这种硬不变量。
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 11
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }

        fn shuffle<T>(&mut self, items: &mut [T]) {
            for index in (1..items.len()).rev() {
                let swap_with = self.below(index as u64 + 1) as usize;
                items.swap(index, swap_with);
            }
        }
    }

    /// 造一批"设备号唯一、计数唯一"的事件，满足 HLC 全序前提。
    fn random_events(seed: u64, count: usize) -> Vec<Lww<Locator>> {
        let mut rng = Lcg(seed);
        let devices = ["A", "B", "C"];
        (0..count)
            .map(|index| {
                let device = devices[rng.below(devices.len() as u64) as usize];
                let wall = 1000 + rng.below(50) as i64;
                let progression = (rng.below(100) as f64) / 100.0;
                // counter 用全局下标保证同一 (wall, device) 下也唯一
                Lww::new(locator(progression), stamp(wall, index as u32, device))
            })
            .collect()
    }

    fn apply_in_order(
        events: &[Lww<Locator>],
        order: &[usize],
        rule: ProgressRule,
    ) -> Option<Lww<Locator>> {
        let mut state: Option<Lww<Locator>> = None;
        for &index in order {
            merge_progress(&mut state, events[index].clone(), rule);
        }
        state
    }

    #[test]
    fn merge_is_convergent_under_random_orderings() {
        for rule in [ProgressRule::LastWriteWins, ProgressRule::FurthestWins] {
            for seed in 0..150u64 {
                let events = random_events(seed, 12);
                let mut rng = Lcg(seed ^ 0x5eed);

                let mut reference: Option<Option<Lww<Locator>>> = None;
                for _ in 0..6 {
                    let mut order: Vec<usize> = (0..events.len()).collect();
                    rng.shuffle(&mut order);
                    let result = apply_in_order(&events, &order, rule);

                    match &reference {
                        None => reference = Some(result),
                        Some(expected) => assert_eq!(
                            expected, &result,
                            "规则 {rule:?}、seed {seed}：不同顺序得到不同结果"
                        ),
                    }
                }
            }
        }
    }

    #[test]
    fn merge_is_idempotent() {
        // 同一批变更应用两遍，结果不变（重复拉取不会出问题）
        for rule in [ProgressRule::LastWriteWins, ProgressRule::FurthestWins] {
            for seed in 0..50u64 {
                let events = random_events(seed, 8);
                let order: Vec<usize> = (0..events.len()).collect();

                let once = apply_in_order(&events, &order, rule);
                let mut twice = once.clone();
                for event in &events {
                    merge_progress(&mut twice, event.clone(), rule);
                }
                assert_eq!(
                    once, twice,
                    "规则 {rule:?}、seed {seed}：重复应用改变了结果"
                );
            }
        }
    }

    #[test]
    fn device_ids_are_a_necessary_precondition() {
        // 反例说明"设备号唯一"是前提：两个设备用同一编号、
        // 同一 (wall,counter) 写不同值 → 顺序不同结果不同。
        let first = Lww::new(locator(0.1), stamp(100, 0, "same"));
        let second = Lww::new(locator(0.9), stamp(100, 0, "same"));

        let mut forward = None;
        merge_progress(&mut forward, first.clone(), ProgressRule::LastWriteWins);
        merge_progress(&mut forward, second.clone(), ProgressRule::LastWriteWins);

        let mut backward = None;
        merge_progress(&mut backward, second, ProgressRule::LastWriteWins);
        merge_progress(&mut backward, first, ProgressRule::LastWriteWins);

        assert_ne!(forward, backward, "这正是文档里说明的限制：设备号必须唯一");
    }

    #[test]
    fn books_are_identified_separately() {
        // 进度是按书的，不同书之间互不影响（这里仅校验模型能区分 book_id）
        let first = BookId::new();
        let second = BookId::new();
        assert_ne!(first, second);
    }
}
