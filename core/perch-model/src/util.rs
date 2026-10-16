//! crate 内部小工具。

/// 供 serde 的 `skip_serializing_if` 使用：`false` 时不写进 JSON，保持输出干净。
pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}
