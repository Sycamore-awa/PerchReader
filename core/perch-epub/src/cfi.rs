//! EPUB CFI（Canonical Fragment Identifier）的解析、生成与规范化序列化。
//!
//! # 为什么需要它
//!
//! CFI 是 EPUB 里唯一能精确定位"某一段文字"的机制，因而是**标注锚点**与**精确进度**的地基
//! （见 `docs/01-里程碑拆解.md` M4："标注不漂移"）。同时它也是跨端同步里最脆弱的一环——
//! 所以本项目对它的策略是：**CFI 精确锚定 + 前后文模糊匹配兜底**（见 `perch-model::Locator`
//! 的 `text.before/after`）。
//!
//! # CFI 的形态
//!
//! ```text
//! epubcfi(/6/4[chap01]!/4/2/2:12)
//!          └──┬──┘       └──┬──┘
//!        指向 spine 中的    内容文档内部的
//!        某个内容文档       元素/文本路径
//! ```
//!
//! 规则要点：
//! - 路径段索引**偶数表示元素子节点，奇数表示字符（文本）节点**；
//! - 索引从 1 开始，`/0` 非法；
//! - `[...]` 是断言（通常是 id），用于在文档结构变化时校验；特殊字符用 `^` 转义；
//! - `!` 分隔"包内路径"与"内容文档内部路径"，最多出现一次；
//! - `:n` 是末尾字符节点内的字符偏移；
//! - `,` 分隔范围定位的各段。
//!
//! # 范围定位的语义（重要，尚未完全实现）
//!
//! 规范里范围形如 `epubcfi(父路径, 起点, 终点)`，后两段是**相对父路径**的；
//! 也常见两段式的简化写法。本模块**只做词法解析**，把各段原样保留在 [`Cfi::parts`] 里，
//! **尚未实现"相对路径 → 绝对路径"的展开**。因此范围定位目前可用于透传与展示，
//! 还不能用来做区间比较。这一点是有意为之：先把点定位做扎实。
//!
//! # 设计取向
//!
//! **解析宽容、校验严格**：解析阶段只拒绝真正非法的输入（空、括号不配对、`/0` 等），
//! 奇偶规则之类的语义约束放在 [`Cfi::validate_indices`] 里由调用方显式检查。
//! 理由是电子书生态里 CFI 的实现普遍不严谨，宁可先收进来再由上层决定怎么处理。

use std::fmt;

use thiserror::Error;

/// CFI 解析/校验错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CfiError {
    /// 输入为空。
    #[error("CFI 为空")]
    Empty,

    /// 逗号分隔出的某一段为空（如 `epubcfi(/6/4,,/6/6)`）。
    #[error("CFI 分段为空")]
    EmptyPart,

    /// 期望单条路径，但输入含逗号分隔的多段。
    #[error("期望单条路径，但输入包含多段")]
    MultipleParts,

    /// `epubcfi(` 没有对应的 `)`。
    #[error("epubcfi( 缺少闭合的 )")]
    UnclosedWrapper,

    /// 方括号不配对，或 `^` 转义不完整。
    #[error("方括号不配对或 ^ 转义不完整")]
    UnbalancedBrackets,

    /// 路径段末尾的断言没有正确闭合。
    #[error("非法断言: {0}")]
    InvalidAssertion(String),

    /// 路径段不是以 `/` 开头。
    #[error("路径必须以 / 开头: {0}")]
    MissingLeadingSlash(String),

    /// 路径段不是合法的"索引[断言]"形式。
    #[error("非法路径段: {0}")]
    InvalidStep(String),

    /// 路径段索引为 0（CFI 索引从 1 开始）。
    #[error("路径段索引不能为 0")]
    ZeroIndex,

    /// 字符偏移不是合法的非负整数。
    #[error("字符偏移非法: {0}")]
    InvalidOffset(String),

    /// `!` 出现了多次。
    #[error("! 最多只能出现一次")]
    MultipleIndirections,

    /// 语义校验：字符节点（奇数索引）后面不能再有子路径段。
    #[error("第 {position} 段是字符节点（索引 {index}），字符节点不能有子节点")]
    CharacterStepNotLast { position: usize, index: u32 },
}

impl CfiError {
    /// 稳定的错误码，跨语言边界后保持不变。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Empty => "CFI_EMPTY",
            Self::EmptyPart => "CFI_EMPTY_PART",
            Self::MultipleParts => "CFI_MULTIPLE_PARTS",
            Self::UnclosedWrapper => "CFI_UNCLOSED_WRAPPER",
            Self::UnbalancedBrackets => "CFI_UNBALANCED_BRACKETS",
            Self::InvalidAssertion(_) => "CFI_INVALID_ASSERTION",
            Self::MissingLeadingSlash(_) => "CFI_MISSING_LEADING_SLASH",
            Self::InvalidStep(_) => "CFI_INVALID_STEP",
            Self::ZeroIndex => "CFI_ZERO_INDEX",
            Self::InvalidOffset(_) => "CFI_INVALID_OFFSET",
            Self::MultipleIndirections => "CFI_MULTIPLE_INDIRECTIONS",
            Self::CharacterStepNotLast { .. } => "CFI_CHARACTER_STEP_NOT_LAST",
        }
    }
}

/// CFI 路径中的一段。
///
/// 索引的奇偶决定它的含义：**偶数为元素子节点，奇数为字符（文本）节点**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// 路径段索引（>= 1）。
    pub index: u32,
    /// 可选断言（通常是元素的 id），已反转义。
    pub assertion: Option<String>,
}

impl Step {
    /// 由索引与断言构造。
    #[must_use]
    pub fn new(index: u32, assertion: Option<String>) -> Self {
        Self { index, assertion }
    }

    /// 由"第 n 个元素子节点"（人类习惯的 1 基序号）构造。
    ///
    /// CFI 用偶数表示元素，所以第 n 个元素对应索引 `2n`。
    #[must_use]
    pub fn element_child(nth: u32) -> Self {
        Self::new(nth * 2, None)
    }

    /// 由"第 n 个字符（文本）节点"（1 基序号）构造。
    ///
    /// CFI 用奇数表示字符节点，所以第 n 个对应索引 `2n - 1`。
    #[must_use]
    pub fn character_child(nth: u32) -> Self {
        Self::new(nth * 2 - 1, None)
    }

    /// 是否为元素子节点（偶数索引）。
    #[must_use]
    pub fn is_element(&self) -> bool {
        self.index % 2 == 0
    }

    /// 是否为字符节点（奇数索引）。
    #[must_use]
    pub fn is_character(&self) -> bool {
        self.index % 2 == 1
    }

    /// 带上断言。
    #[must_use]
    pub fn with_assertion(mut self, assertion: impl Into<String>) -> Self {
        self.assertion = Some(assertion.into());
        self
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.index)?;
        if let Some(assertion) = &self.assertion {
            write!(f, "[{}]", escape_assertion(assertion))?;
        }
        Ok(())
    }
}

/// 一条 CFI 路径：可选的"指向内容文档的包内路径" + "内容文档内部路径" + 可选字符偏移。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CfiPath {
    /// `!` 之前的路径，指向 spine 中的某个内容文档。
    pub indirection: Vec<Step>,
    /// `!` 之后的路径，位于内容文档内部。
    pub local: Vec<Step>,
    /// 末尾字符节点内的字符偏移（`:n`）。
    pub text_offset: Option<u32>,
}

impl CfiPath {
    /// 构造。
    #[must_use]
    pub fn new(indirection: Vec<Step>, local: Vec<Step>, text_offset: Option<u32>) -> Self {
        Self {
            indirection,
            local,
            text_offset,
        }
    }

    /// 解析单条路径（不含 `epubcfi()` 包裹，也不含逗号分隔的多段）。
    ///
    /// 与 [`Cfi::parse`] 的区别：这里要求输入恰好是一条路径。
    ///
    /// # Errors
    ///
    /// 路径非法，或输入含多段时返回 [`CfiError::MultipleParts`]。
    pub fn parse(input: &str) -> Result<Self, CfiError> {
        let mut parts = Cfi::parse(input)?.parts;
        if parts.len() != 1 {
            return Err(CfiError::MultipleParts);
        }
        match parts.pop() {
            Some(path) => Ok(path),
            None => Err(CfiError::Empty),
        }
    }

    /// 是否没有任何路径段。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indirection.is_empty() && self.local.is_empty()
    }

    /// 去掉最后一段，得到父路径。
    ///
    /// 用来实现"从标注位置回溯到其所在块"之类的操作。
    /// 注意：只删除 `local` 的末段；`local` 为空时删除 `indirection` 的末段。
    /// 剥到空路径时返回 `None`（**不返回空路径**——空路径无法再被解析回来，
    /// 返回一个"存在但不可用"的值比返回 `None` 更容易埋坑）。
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        let mut next = self.clone();
        let popped = if next.local.pop().is_some() {
            true
        } else {
            next.indirection.pop().is_some()
        };
        if !popped || next.is_empty() {
            return None;
        }
        Some(next)
    }

    /// 设置字符偏移。
    #[must_use]
    pub fn with_text_offset(mut self, offset: u32) -> Self {
        self.text_offset = Some(offset);
        self
    }

    /// 语义校验：字符节点（奇数索引）必须是该部分的最后一段（文本节点没有子节点）。
    ///
    /// # Errors
    ///
    /// 违反奇偶约束时返回 [`CfiError::CharacterStepNotLast`]。
    pub fn validate_indices(&self) -> Result<(), CfiError> {
        for steps in [&self.indirection, &self.local] {
            let last = steps.len().saturating_sub(1);
            for (position, step) in steps.iter().enumerate() {
                if step.is_character() && position != last {
                    return Err(CfiError::CharacterStepNotLast {
                        position: position + 1,
                        index: step.index,
                    });
                }
            }
        }
        Ok(())
    }
}

impl fmt::Display for CfiPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for step in &self.indirection {
            write!(f, "/{step}")?;
        }
        // 只要出现过分段指示（indirection 非空），就保留 `!`，
        // 以保证 `a!b` 这类写法能稳定地往返。
        if !self.indirection.is_empty() {
            f.write_str("!")?;
        }
        for step in &self.local {
            write!(f, "/{step}")?;
        }
        if let Some(offset) = self.text_offset {
            write!(f, ":{offset}")?;
        }
        Ok(())
    }
}

/// 一个 CFI：逗号分隔的一到多段。
///
/// - 一段：点定位（进度、书签）
/// - 多段：范围定位（高亮）。语义见模块文档的说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cfi {
    /// 各段，顺序与输入一致。
    pub parts: Vec<CfiPath>,
}

impl Cfi {
    /// 由单个点定位路径构造。
    #[must_use]
    pub fn point(path: CfiPath) -> Self {
        Self { parts: vec![path] }
    }

    /// 解析 CFI 文本。`epubcfi(...)` 包裹可有可无。
    ///
    /// # Errors
    ///
    /// 见 [`CfiError`]。解析只拒绝真正非法的输入；奇偶等语义约束请另外调用
    /// [`Cfi::validate_indices`]。
    pub fn parse(input: &str) -> Result<Self, CfiError> {
        let inner = strip_wrapper(input)?;
        if inner.trim().is_empty() {
            return Err(CfiError::Empty);
        }
        check_brackets(inner)?;

        let mut parts = Vec::new();
        for chunk in split_top_level(inner, ',') {
            let chunk = chunk.trim();
            if chunk.is_empty() {
                return Err(CfiError::EmptyPart);
            }
            parts.push(parse_path(chunk)?);
        }
        if parts.is_empty() {
            return Err(CfiError::Empty);
        }
        Ok(Self { parts })
    }

    /// 是否为范围定位（多于一段）。
    #[must_use]
    pub fn is_range(&self) -> bool {
        self.parts.len() > 1
    }

    /// 第一段。点定位时即它本身；范围定位时按规范是**父路径**。
    #[must_use]
    pub fn primary(&self) -> Option<&CfiPath> {
        self.parts.first()
    }

    /// 对每一段做奇偶语义校验。
    ///
    /// # Errors
    ///
    /// 任一段违反约束即返回错误。
    pub fn validate_indices(&self) -> Result<(), CfiError> {
        for part in &self.parts {
            part.validate_indices()?;
        }
        Ok(())
    }

    /// 带 `epubcfi(...)` 包裹的规范形式。
    #[must_use]
    pub fn wrapped(&self) -> String {
        format!("epubcfi({self})")
    }
}

impl fmt::Display for Cfi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, part) in self.parts.iter().enumerate() {
            if index > 0 {
                f.write_str(",")?;
            }
            write!(f, "{part}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for Cfi {
    type Err = CfiError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse(input)
    }
}

/// 去掉可选的 `epubcfi(...)` 包裹。
fn strip_wrapper(input: &str) -> Result<&str, CfiError> {
    let trimmed = input.trim();
    match trimmed.strip_prefix("epubcfi(") {
        Some(rest) => rest.strip_suffix(')').ok_or(CfiError::UnclosedWrapper),
        None => Ok(trimmed),
    }
}

/// 检查方括号配对与 `^` 转义完整性。
fn check_brackets(input: &str) -> Result<(), CfiError> {
    let mut depth: i32 = 0;
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '^' => escaped = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth < 0 {
                    return Err(CfiError::UnbalancedBrackets);
                }
            }
            _ => {}
        }
    }
    if depth != 0 || escaped {
        return Err(CfiError::UnbalancedBrackets);
    }
    Ok(())
}

/// 在**顶层**（不在 `[...]` 内、未被 `^` 转义）按分隔符切分。
fn split_top_level(input: &str, delimiter: char) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth: usize = 0;
    let mut escaped = false;
    let mut start = 0usize;

    for (index, ch) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '^' => escaped = true,
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            _ if ch == delimiter && depth == 0 => {
                result.push(&input[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    result.push(&input[start..]);
    result
}

/// 在顶层按分隔符切分**一次**，返回前半与后半。
fn split_once_top_level(input: &str, delimiter: char) -> Option<(&str, &str)> {
    let mut depth: usize = 0;
    let mut escaped = false;

    for (index, ch) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '^' => escaped = true,
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            _ if ch == delimiter && depth == 0 => {
                return Some((&input[..index], &input[index + ch.len_utf8()..]));
            }
            _ => {}
        }
    }
    None
}

/// 解析一条路径（可能含 `!` 与 `:n`）。
fn parse_path(input: &str) -> Result<CfiPath, CfiError> {
    let (path_part, offset_part) = match split_once_top_level(input, ':') {
        Some((path, offset)) => (path, Some(offset)),
        None => (input, None),
    };

    let text_offset = match offset_part {
        Some(raw) => Some(
            raw.trim()
                .parse::<u32>()
                .map_err(|_| CfiError::InvalidOffset(raw.to_owned()))?,
        ),
        None => None,
    };

    let (indirection, local) = match split_once_top_level(path_part, '!') {
        Some((head, tail)) => {
            if split_once_top_level(tail, '!').is_some() {
                return Err(CfiError::MultipleIndirections);
            }
            (parse_steps(head)?, parse_steps(tail)?)
        }
        None => (Vec::new(), parse_steps(path_part)?),
    };

    let result = CfiPath {
        indirection,
        local,
        text_offset,
    };
    if result.is_empty() {
        return Err(CfiError::EmptyPart);
    }
    Ok(result)
}

/// 解析一连串 `/` 分隔的路径段。
fn parse_steps(input: &str) -> Result<Vec<Step>, CfiError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if !trimmed.starts_with('/') {
        return Err(CfiError::MissingLeadingSlash(trimmed.to_owned()));
    }
    let mut steps = Vec::new();
    for chunk in split_top_level(trimmed, '/') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        steps.push(parse_step(chunk)?);
    }
    Ok(steps)
}

/// 解析单个路径段：`索引` 或 `索引[断言]`。
fn parse_step(chunk: &str) -> Result<Step, CfiError> {
    let split = chunk
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(chunk.len());
    let (digits, rest) = chunk.split_at(split);

    if digits.is_empty() {
        return Err(CfiError::InvalidStep(chunk.to_owned()));
    }
    let index: u32 = digits
        .parse()
        .map_err(|_| CfiError::InvalidStep(chunk.to_owned()))?;
    if index == 0 {
        return Err(CfiError::ZeroIndex);
    }

    let assertion = if rest.is_empty() {
        None
    } else {
        let inner = rest
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .ok_or_else(|| CfiError::InvalidAssertion(chunk.to_owned()))?;
        Some(unescape_assertion(inner)?)
    };

    Ok(Step { index, assertion })
}

/// 给断言里的特殊字符加 `^` 转义。
fn escape_assertion(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if matches!(ch, '^' | '[' | ']' | '(' | ')' | ',') {
            out.push('^');
        }
        out.push(ch);
    }
    out
}

/// 去掉断言里的 `^` 转义。
fn unescape_assertion(raw: &str) -> Result<String, CfiError> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch == '^' {
            out.push(chars.next().ok_or(CfiError::UnbalancedBrackets)?);
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn parses_typical_readium_cfi() {
        let cfi = Cfi::parse("epubcfi(/6/4[chap01]!/4/2/2:12)").unwrap();
        assert!(!cfi.is_range());
        let path = cfi.primary().unwrap();

        assert_eq!(
            path.indirection,
            vec![Step::new(6, None), Step::new(4, Some("chap01".to_owned())),]
        );
        assert_eq!(
            path.local,
            vec![Step::new(4, None), Step::new(2, None), Step::new(2, None)]
        );
        assert_eq!(path.text_offset, Some(12));
        cfi.validate_indices().unwrap();
    }

    #[test]
    fn accepts_bare_path_without_wrapper() {
        let bare = Cfi::parse("/6/4!/4/2:0").unwrap();
        let wrapped = Cfi::parse("epubcfi(/6/4!/4/2:0)").unwrap();
        assert_eq!(bare, wrapped);
        assert_eq!(bare.to_string(), "/6/4!/4/2:0");
        assert_eq!(bare.wrapped(), "epubcfi(/6/4!/4/2:0)");
        // 两者互相往返
        assert_eq!(Cfi::parse(&bare.wrapped()).unwrap(), bare);
    }

    #[test]
    fn round_trips_canonical_forms() {
        for raw in [
            "/6/4[chap01]!/4/2/2:12",
            "/6/4!/4/2",
            "/6/4",
            "/2/2/2:0",
            "/6/4[sec^,one]!/4/2:5",
        ] {
            let cfi = Cfi::parse(raw).unwrap();
            let rendered = cfi.to_string();
            assert_eq!(rendered, raw, "规范化序列化应当稳定");
            assert_eq!(Cfi::parse(&rendered).unwrap(), cfi);
        }
    }

    #[test]
    fn builds_cfi_from_dom_child_numbers() {
        // 人类习惯：第 2 个元素子节点 → CFI 索引 4
        assert_eq!(Step::element_child(2).index, 4);
        assert_eq!(Step::character_child(1).index, 1);
        assert_eq!(Step::character_child(3).index, 5);

        let path = CfiPath::new(
            vec![Step::element_child(3)], // 第 3 个 spine 项 → /6
            vec![Step::element_child(2), Step::character_child(1)],
            Some(7),
        );
        assert_eq!(path.to_string(), "/6!/4/1:7");
        path.validate_indices().unwrap();
    }

    #[test]
    fn assertion_escaping_round_trips() {
        let cfi = Cfi::parse("epubcfi(/6/4[a^[b^]c^,d^)e^(f]!/4/2)").unwrap();
        let assertion = cfi.primary().unwrap().indirection[1]
            .assertion
            .clone()
            .unwrap();
        assert_eq!(assertion, "a[b]c,d)e(f");
        // 重新序列化必须转义回去
        assert_eq!(cfi.to_string(), "/6/4[a^[b^]c^,d^)e^(f]!/4/2");
        assert_eq!(Cfi::parse(&cfi.to_string()).unwrap(), cfi);
    }

    #[test]
    fn range_cfi_is_kept_as_parts() {
        let cfi = Cfi::parse("epubcfi(/6/4!/4/2,/6/4!/4/6)").unwrap();
        assert!(cfi.is_range());
        assert_eq!(cfi.parts.len(), 2);
        // 两段各自的文本偏移
        let cfi = Cfi::parse("epubcfi(/6/4!/4/2/2:0,/6/4!/4/2/2:20)").unwrap();
        assert_eq!(cfi.parts[0].text_offset, Some(0));
        assert_eq!(cfi.parts[1].text_offset, Some(20));
    }

    #[test]
    fn rejects_malformed_input() {
        let cases: &[(&str, &str)] = &[
            ("", "CFI_EMPTY"),
            ("   ", "CFI_EMPTY"),
            ("epubcfi(/6/4", "CFI_UNCLOSED_WRAPPER"),
            ("6/4", "CFI_MISSING_LEADING_SLASH"),
            ("/6/0", "CFI_ZERO_INDEX"),
            ("/0", "CFI_ZERO_INDEX"),
            ("/6/4[unclosed", "CFI_UNBALANCED_BRACKETS"),
            ("/6/4]bad]", "CFI_UNBALANCED_BRACKETS"),
            ("/6/4[a^", "CFI_UNBALANCED_BRACKETS"),
            ("/6/4!/4/2!/6", "CFI_MULTIPLE_INDIRECTIONS"),
            ("/6/4!/4/2:abc", "CFI_INVALID_OFFSET"),
            ("/6/4!/4/2:", "CFI_INVALID_OFFSET"),
            ("/abc", "CFI_INVALID_STEP"),
            ("/6/4,,/6/6", "CFI_EMPTY_PART"),
            ("/6/4!/4/2[x]y", "CFI_INVALID_ASSERTION"),
        ];
        for (input, expected) in cases {
            let error = Cfi::parse(input).unwrap_err();
            assert_eq!(error.code(), *expected, "输入 = {input:?}");
        }
    }

    #[test]
    fn rejects_children_of_character_nodes() {
        // /3 是字符节点，后面不能再有 /2
        let cfi = Cfi::parse("/6/3/2").unwrap();
        let error = cfi.validate_indices().unwrap_err();
        assert_eq!(error.code(), "CFI_CHARACTER_STEP_NOT_LAST");

        // 字符节点在末尾是合法的
        Cfi::parse("/6/4/3").unwrap().validate_indices().unwrap();
        // 全是元素节点也合法
        Cfi::parse("/2/4/6").unwrap().validate_indices().unwrap();
    }

    #[test]
    fn parent_peels_last_step() {
        let cfi = Cfi::parse("/6/4!/4/2/2:12").unwrap();
        let path = cfi.primary().unwrap();

        let parent = path.parent().unwrap();
        assert_eq!(parent.to_string(), "/6/4!/4/2:12");
        let grandparent = parent.parent().unwrap();
        assert_eq!(grandparent.to_string(), "/6/4!/4:12");
        // local 段清空后开始剥 indirection；剥到空则返回 None
        let up = CfiPath::new(vec![Step::new(6, None)], Vec::new(), None);
        assert!(up.parent().is_none(), "剥到空路径应返回 None");
        assert!(CfiPath::default().parent().is_none());
    }

    #[test]
    fn step_kind_follows_parity() {
        assert!(Step::element_child(1).is_element());
        assert!(!Step::element_child(1).is_character());
        assert!(Step::character_child(1).is_character());
        assert!(!Step::character_child(1).is_element());
    }

    #[test]
    fn from_str_works() {
        let cfi: Cfi = "/6/4!/4/2".parse().unwrap();
        assert_eq!(cfi.to_string(), "/6/4!/4/2");
    }

    #[test]
    fn multiple_text_offsets_are_rejected() {
        // 第二个冒号会落到偏移解析里，必须报错而不是被忽略
        let error = Cfi::parse("/6/4!/4/2:1:2").unwrap_err();
        assert_eq!(error.code(), "CFI_INVALID_OFFSET");
    }
}
