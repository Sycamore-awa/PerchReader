//! 混合逻辑时钟（Hybrid Logical Clock, HLC）。
//!
//! # 为什么不用设备本地时间戳
//!
//! 手机时钟不准、可被用户改、可跨时区。若用 `Date.now()` 做"谁更新"的判据，
//! 一台慢了 5 分钟的设备写的东西会被永远忽略——数据悄悄丢失，而用户完全不知道。
//!
//! # HLC 怎么解决
//!
//! 每个时间戳是 `(物理毫秒, 逻辑计数, 设备号)`：
//!
//! - **物理毫秒**：尽量贴近真实时间，便于人读与调试
//! - **逻辑计数**：同一毫秒内发生的多个事件靠它定序，也保证"时钟倒退时仍然单调"
//! - **设备号**：最后的总序 tie-break，让不同设备在同一毫秒的事件也有确定顺序
//!
//! 关键性质：**任意两个事件的 HLC 都能比较出确定的大小**（前提是设备号唯一），
//! 因此"谁更新"这件事在**所有设备上算出同一个答案**——这是同步收敛的基础。
//!
//! # 文本格式
//!
//! `{wall_ms}-{counter}-{device}`，例如 `1760600000000-3-phoneA`。
//! 与 `perch-model::Locator` 里 `perch.hlc` 字段的约定一致。

use std::cmp::Ordering;
use std::fmt;

/// 混合逻辑时钟时间戳。
///
/// 排序规则：先比物理毫秒，再比逻辑计数，最后比设备号。
/// 三者合起来给出**全序**，因此合并结果与到达顺序无关。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hlc {
    /// 物理毫秒（Unix 纪元）。
    wall_ms: i64,
    /// 同一毫秒内的逻辑计数。
    counter: u32,
    /// 设备标识（必须在同一账号的所有设备间唯一）。
    device: String,
}

impl Hlc {
    /// 创建一个起点时钟。
    #[must_use]
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            wall_ms: 0,
            counter: 0,
            device: device.into(),
        }
    }

    /// 由已知的三元组直接构造（主要用于解析与测试）。
    #[must_use]
    pub fn from_parts(wall_ms: i64, counter: u32, device: impl Into<String>) -> Self {
        Self {
            wall_ms,
            counter,
            device: device.into(),
        }
    }

    /// 物理毫秒。
    #[must_use]
    pub const fn wall_ms(&self) -> i64 {
        self.wall_ms
    }

    /// 逻辑计数。
    #[must_use]
    pub const fn counter(&self) -> u32 {
        self.counter
    }

    /// 设备号。
    #[must_use]
    pub fn device(&self) -> &str {
        &self.device
    }

    /// 本地事件：推进时钟并返回本次事件的时间戳。
    ///
    /// 若 `now_ms` 比已知的物理时间更早（时钟倒退），**不会**让时间戳变小：
    /// 而是保持原来的物理时间、把逻辑计数加一。这样单调性永远成立。
    pub fn tick(&mut self, now_ms: i64) -> Self {
        if now_ms > self.wall_ms {
            self.wall_ms = now_ms;
            self.counter = 0;
        } else {
            // 同一毫秒（或时钟倒退）→ 靠逻辑计数保持单调
            self.counter = self.counter.saturating_add(1);
        }
        self.clone()
    }

    /// 收到远端时间戳：把自己的时钟推进到"不少于对方"，并返回本地新事件的时间戳。
    ///
    /// 这是 HLC 的标准 receive 算法：取三方最大值，若与某方同毫秒则在此基础上加一。
    pub fn observe(&mut self, remote: &Self, now_ms: i64) -> Self {
        let wall = self.wall_ms.max(remote.wall_ms).max(now_ms);
        let counter = if wall == self.wall_ms && wall == remote.wall_ms {
            self.counter.max(remote.counter).saturating_add(1)
        } else if wall == self.wall_ms {
            self.counter.saturating_add(1)
        } else if wall == remote.wall_ms {
            remote.counter.saturating_add(1)
        } else {
            0
        };
        self.wall_ms = wall;
        self.counter = counter;
        self.clone()
    }

    /// 解析文本格式。设备号里允许出现 `-`（只按前两个 `-` 切分）。
    ///
    /// # Errors
    ///
    /// 格式不合法时返回 `None`。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.splitn(3, '-');
        let wall_ms = parts.next()?.parse::<i64>().ok()?;
        let counter = parts.next()?.parse::<u32>().ok()?;
        let device = parts.next()?;
        if device.is_empty() {
            return None;
        }
        Some(Self {
            wall_ms,
            counter,
            device: device.to_owned(),
        })
    }
}

impl fmt::Display for Hlc {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}-{}-{}",
            self.wall_ms, self.counter, self.device
        )
    }
}

// HLC 在线上用它的文本形式（`{wall_ms}-{counter}-{device}`）传输：
// 与 `Display`/`parse` 完全一致，JSON 里可读、体积也小，
// 不需要再维护第二套二进制布局。
impl serde::Serialize for Hlc {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Hlc {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| serde::de::Error::custom("非法的 HLC 文本"))
    }
}

impl Ord for Hlc {
    fn cmp(&self, other: &Self) -> Ordering {
        self.wall_ms
            .cmp(&other.wall_ms)
            .then_with(|| self.counter.cmp(&other.counter))
            .then_with(|| self.device.cmp(&other.device))
    }
}

impl PartialOrd for Hlc {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn tick_advances_wall_clock() {
        let mut clock = Hlc::new("A");
        let first = clock.tick(1000);
        assert_eq!(first.wall_ms(), 1000);
        assert_eq!(first.counter(), 0);

        let second = clock.tick(2000);
        assert_eq!(second.wall_ms(), 2000);
        assert_eq!(second.counter(), 0);
    }

    #[test]
    fn tick_stays_monotonic_when_clock_goes_backwards() {
        let mut clock = Hlc::new("A");
        let first = clock.tick(5000);
        // 设备时钟被改到过去
        let second = clock.tick(1000);
        assert!(second > first, "时钟倒退时时间戳仍必须递增");
        assert_eq!(second.wall_ms(), 5000, "物理时间不倒退");
        assert_eq!(second.counter(), 1, "靠逻辑计数前进");
    }

    #[test]
    fn tick_is_strictly_increasing_within_one_millisecond() {
        let mut clock = Hlc::new("A");
        let mut previous = clock.tick(1000);
        for _ in 0..50 {
            let next = clock.tick(1000);
            assert!(next > previous);
            previous = next;
        }
        assert_eq!(previous.counter(), 50);
    }

    #[test]
    fn observe_pulls_local_clock_forward() {
        let mut clock = Hlc::new("A");
        clock.tick(1000);
        let remote = Hlc::from_parts(9000, 4, "B");
        let merged = clock.observe(&remote, 1500);
        assert!(merged > remote, "合并后必须严格大于远端");
        assert_eq!(merged.wall_ms(), 9000);
        assert_eq!(merged.counter(), 5, "同一毫秒内取远端计数 +1");
    }

    #[test]
    fn observe_uses_local_time_when_local_is_ahead() {
        let mut clock = Hlc::new("A");
        clock.tick(9000);
        let remote = Hlc::from_parts(1000, 0, "B");
        let merged = clock.observe(&remote, 1200);
        assert!(merged > remote);
        assert_eq!(merged.wall_ms(), 9000);
    }

    #[test]
    fn observe_handles_same_millisecond() {
        let mut clock = Hlc::from_parts(1000, 3, "A");
        let remote = Hlc::from_parts(1000, 7, "B");
        let merged = clock.observe(&remote, 1000);
        assert_eq!(merged.wall_ms(), 1000);
        assert_eq!(merged.counter(), 8, "max(3,7)+1");
    }

    #[test]
    fn display_and_parse_round_trip() {
        let original = Hlc::from_parts(1_760_600_000_000, 12, "phoneA");
        let text = original.to_string();
        assert_eq!(text, "1760600000000-12-phoneA");
        assert_eq!(Hlc::parse(&text).unwrap(), original);
    }

    #[test]
    fn parse_allows_dashes_in_device_name() {
        let parsed = Hlc::parse("100-2-my-phone-A").unwrap();
        assert_eq!(parsed.device(), "my-phone-A");
        assert_eq!(parsed.counter(), 2);
    }

    #[test]
    fn parse_rejects_malformed() {
        for bad in ["", "abc", "100-2", "100-2-", "x-1-A", "1-y-A"] {
            assert!(Hlc::parse(bad).is_none(), "应拒绝 {bad:?}");
        }
    }

    #[test]
    fn ordering_is_wall_then_counter_then_device() {
        let a = Hlc::from_parts(100, 0, "A");
        let b = Hlc::from_parts(100, 1, "A");
        let c = Hlc::from_parts(100, 1, "B");
        let d = Hlc::from_parts(101, 0, "A");
        assert!(a < b);
        assert!(b < c, "同毫秒同计数时按设备号定序");
        assert!(c < d);
    }
}
