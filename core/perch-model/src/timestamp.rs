use serde::{Deserialize, Serialize};

/// Unix 纪元以来的**毫秒**数（UTC）。
///
/// 刻意不用字符串或第三方时间类型，原因：
/// - 与混合逻辑时钟（HLC）的 `wall_ms` 表示一致，同步层无需转换；
/// - 零第三方依赖，跨语言映射简单（Dart `int` / Swift `Int64` / Kotlin `Long`）；
/// - 整数可直接比较排序，做 LWW 合并时无需解析。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    /// 由毫秒时间戳构造。
    #[must_use]
    pub const fn from_millis(millis: i64) -> Self {
        Self(millis)
    }

    /// 取毫秒时间戳。
    #[must_use]
    pub const fn as_millis(self) -> i64 {
        self.0
    }

    /// 当前时间。
    ///
    /// 若系统时钟早于 Unix 纪元（理论上可能，例如用户手动改错时间），返回 0 而非 panic：
    /// **时间戳不可信不应导致崩溃**。
    #[must_use]
    pub fn now() -> Self {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self(millis)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn roundtrip_millis() {
        let ts = Timestamp::from_millis(1_700_000_000_000);
        assert_eq!(ts.as_millis(), 1_700_000_000_000);
    }

    #[test]
    fn serializes_as_plain_integer() {
        let ts = Timestamp::from_millis(42);
        assert_eq!(serde_json::to_string(&ts).unwrap(), "42");
    }

    #[test]
    fn now_is_after_year_2020() {
        // 若返回 0，说明时钟异常；正常情况应远大于 2020-01-01
        let now = Timestamp::now();
        assert!(
            now.as_millis() > 1_577_836_800_000,
            "now = {}",
            now.as_millis()
        );
    }

    #[test]
    fn ordering_is_numeric() {
        let a = Timestamp::from_millis(100);
        let b = Timestamp::from_millis(200);
        assert!(a < b);
        assert_eq!(a.max(b), b);
    }
}
