use thiserror::Error;

/// 模型层错误。
///
/// 每个变体都提供稳定的 [`ModelError::code`]，供跨语言层（FFI）映射为错误码字符串：
/// 上层 UI 可以针对错误类型分支处理，而不必依赖随时可能改动的错误文案。
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ModelError {
    /// 必填字段是空字符串。
    #[error("字段不能为空: {field}")]
    EmptyField { field: &'static str },

    /// 进度值越界，或为 NaN / 无穷。
    #[error("进度值非法（须在 0.0~1.0 之间且为有限数）: {field} = {value}")]
    InvalidProgression { field: &'static str, value: f64 },

    /// Locator 缺少该格式所必需的定位锚点。
    #[error("Locator 缺少定位信息：格式 {format} 需要 {required}")]
    MissingAnchor {
        format: &'static str,
        required: &'static str,
    },

    /// 归一化矩形非法。
    #[error("矩形非法（数值须有限、宽高须为正、分量须在 0.0~1.0 之间）: {rect:?}")]
    InvalidRect { rect: [f32; 4] },

    /// 标识符无法解析。
    #[error("标识符非法: {kind} = {value}")]
    InvalidId { kind: &'static str, value: String },

    /// 导入状态机的非法迁移。
    #[error("非法的导入状态迁移: {from} -> {to}")]
    InvalidTransition {
        from: &'static str,
        to: &'static str,
    },

    /// 标注范围非法（结束位置早于开始位置）。
    #[error("标注范围非法：结束位置早于开始位置")]
    InvalidRange,

    /// 标注形状非法（书签带结束位置、范围标注缺结束位置、笔记缺正文、范围跨资源等）。
    #[error("标注形状非法：{kind} {detail}")]
    InvalidAnnotationShape {
        kind: &'static str,
        detail: &'static str,
    },
}

impl ModelError {
    /// 稳定的错误码，跨越语言边界后保持不变。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::EmptyField { .. } => "MODEL_EMPTY_FIELD",
            Self::InvalidProgression { .. } => "MODEL_INVALID_PROGRESSION",
            Self::MissingAnchor { .. } => "MODEL_MISSING_ANCHOR",
            Self::InvalidRect { .. } => "MODEL_INVALID_RECT",
            Self::InvalidId { .. } => "MODEL_INVALID_ID",
            Self::InvalidTransition { .. } => "MODEL_INVALID_TRANSITION",
            Self::InvalidRange => "MODEL_INVALID_RANGE",
            Self::InvalidAnnotationShape { .. } => "MODEL_INVALID_ANNOTATION_SHAPE",
        }
    }
}
