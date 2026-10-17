//! 标注锚点的**重定位**：当 CFI 失效时，靠原文上下文把标注找回来。
//!
//! # 为什么需要它
//!
//! EPUB 的 CFI 只是"结构路径"，它假定了内容文档的 DOM 结构不变。但现实中：
//!
//! - 用户调整字号 / 行距 → 分页变了（不影响 CFI，但影响渲染位置）
//! - 换了同名的另一版电子书 → 章节可能被重新拆分
//! - 出版社勘误、译者修订 → 段落增删，后续所有 CFI 全部错位
//!
//! 这些情况下 CFI 会指向**错误的位置，甚至完全找不到**。若只靠 CFI，
//! 用户会看到高亮标示在完全无关的句子上——这比丢失更难接受。
//!
//! 所以 `Locator` 里额外保存了三段原文（见 `perch-model` 的 `TextContext`）：
//!
//! ```text
//! …前文… [highlight] …后文…
//! ```
//!
//! 本模块用它们做**模糊匹配**：在内容文档的全文里找出最像"那个位置"的地方。
//!
//! # 算法
//!
//! 1. **精确查找**所有 `highlight` 出现的位置（可能多处）
//! 2. 对每个候选按**上下文吻合度**打分：前文能对上几个字、后文能对上几个字
//! 3. 取得分最高者；低于阈值就判定失败（宁可报"找不到"，也不要指到别处）
//! 4. 精确查找失败时，退化为**忽略空白差异**的匹配
//!    —— 这是现实中最常见的失效原因（重新排版会改变换行与空格）
//!
//! # 设计取向：宁缺毋滥
//!
//! 标注指错位置比找不到更糟：用户会以为自己的笔记被篡改了。
//! 因此置信度低于阈值时返回 `None`，由上层降级到"章节 + 大致进度"。
//!
//! # 偏移口径
//!
//! 全部是**字符**偏移（不是字节），与 `perch-model::Locator` 的约定一致。
//! 中文一个字 3 字节，按字节算会错得很离谱。

use std::ops::Range;

/// 重定位的输入：一段被标注的原文 + 它前后的上下文。
#[derive(Debug, Clone, Copy)]
pub struct AnchorQuery<'a> {
    /// 被标注的原文。
    pub highlight: &'a str,
    /// 标注位置**之前**的一段原文。
    pub before: Option<&'a str>,
    /// 标注位置**之后**的一段原文。
    pub after: Option<&'a str>,
}

/// 重定位结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Anchored {
    /// 命中区间的字符范围。
    pub range: Range<u64>,
    /// 置信度 0~1。低于 [`MIN_CONFIDENCE`] 的不会被返回。
    pub confidence: f32,
    /// 是否是忽略空白差异后匹配到的（说明原文的空白被改过）。
    pub whitespace_tolerant: bool,
}

/// 上下文最多取多少字符（对齐 Hypothesis 的 `contextLen = 32`）。
///
/// 前文越长，越容易在无关区域"碰巧"匹配；固定上限把误命中率压下来。
/// Hypothesis 的注释也承认这是权宜之计（理想做法是按段落/文章边界截断）。
pub const CONTEXT_CHARS: usize = 32;

/// 置信度下限。低于它的结果一律丢弃。
///
/// 0.6 的含义：至少要有"正好命中原文 + 一点上下文佐证"，
/// 或者"唯一出现但完全没有上下文"。
pub const MIN_CONFIDENCE: f32 = 0.6;

/// 在文档全文中重定位一个标注。
///
/// `document` 是内容文档的**纯文本**（由 `XhtmlIndex` 拼接各文本节点得到）。
#[must_use]
pub fn reanchor(document: &str, query: &AnchorQuery<'_>) -> Option<Anchored> {
    reanchor_with_hint(document, query, None)
}

/// 带**位置先验**的重定位。
///
/// `hint` 是一个大致字符偏移（由 CFI 或 progression 换算而来），用作
/// **平局裁决**：当同一段文字在正文里出现多次、而上下文又不足以区分时，
/// 靠近 `hint` 的那个候选胜出。
///
/// 这一项来自 Hypothesis 的做法（`TextPositionSelector` → `TextQuoteSelector`
/// 的 hint，权重仅 2/92 ≈ 2.2%）。权重很弱，但恰好够用来打破平局——
/// 而"同一句话出现两次"正是真实语料里最常见的歧义来源。
#[must_use]
pub fn reanchor_with_hint(
    document: &str,
    query: &AnchorQuery<'_>,
    hint: Option<u64>,
) -> Option<Anchored> {
    let highlight = query.highlight.trim();
    if highlight.is_empty() || document.is_empty() {
        return None;
    }

    // 先试精确匹配
    if let Some(found) = match_exact(document, highlight, query, hint) {
        return Some(found);
    }

    // 精确匹配不到 → 退化为忽略空白差异
    match_ignoring_whitespace(document, highlight, query)
}

/// 精确匹配：找出所有出现位置并按上下文打分。
fn match_exact(
    document: &str,
    highlight: &str,
    query: &AnchorQuery<'_>,
    hint: Option<u64>,
) -> Option<Anchored> {
    let mut best: Option<Anchored> = None;
    let occurrences: Vec<usize> = document
        .match_indices(highlight)
        .map(|(at, _)| at)
        .collect();

    for &byte_start in &occurrences {
        let char_start = document
            .get(..byte_start)
            .map(|prefix| prefix.chars().count() as u64)
            .unwrap_or(0);
        let char_end = char_start + highlight.chars().count() as u64;
        let confidence = score(
            document,
            char_start,
            char_end,
            query,
            occurrences.len(),
            hint,
        );
        let candidate = Anchored {
            range: char_start..char_end,
            confidence,
            whitespace_tolerant: false,
        };
        if best
            .as_ref()
            .is_none_or(|existing| candidate.confidence > existing.confidence)
        {
            best = Some(candidate);
        }
    }
    best.filter(|found| found.confidence >= MIN_CONFIDENCE)
}

/// 计算某个候选位置的置信度。
///
/// # 证据模型
///
/// 四个证据项，每项权重相同（0.25），"命中原文"本身占 0.5 作为基础：
///
/// | 证据 | 含义 |
/// |---|---|
/// | 命中原文 | 必然成立（候选就是这么来的），给 0.5 |
/// | 前文吻合 | 紧贴标注之前的 32 字符，从尾往前比 |
/// | 后文吻合 | 紧贴标注之后的 32 字符，从头往后比 |
/// | 位置先验 | 候选离 `hint` 越近越可信 |
///
/// 若**除原文外没有任何其它证据**（既无上下文也无 hint），则退回"唯一性"判断：
/// 全文只出现一次 → 0.8，出现多次 → 0.4（低于阈值被丢弃）。
///
/// # 与 Hypothesis 的差异（有意为之）
///
/// Hypothesis 用 50/20/20/2 的线性加权，其中 quote 项是
/// `1 - errors/len`（基于编辑距离的连续值）。我们的 quote 项是**二值**的
/// （精确命中或不做），所以直接套用 50/92 会让"精确命中"占 54%，
/// 与"编辑距离允许一半字符出错"的语义不可比。因此改用等权重结构：
/// 保留它"四类证据 + hint 作先验"的**结构**，不照搬不可比的**数值**。
fn score(
    document: &str,
    char_start: u64,
    char_end: u64,
    query: &AnchorQuery<'_>,
    occurrence_count: usize,
    hint: Option<u64>,
) -> f32 {
    let before = truncate_context(query.before.unwrap_or("").trim_end(), true);
    let after = truncate_context(query.after.unwrap_or("").trim_start(), false);

    let mut evidence = 0.5f32; // 命中原文
    let mut terms = 1usize;

    if !before.is_empty() {
        // 前文：紧贴标注**之前**的文字，从尾部往前比
        let actual_start = char_start.saturating_sub(before.len() as u64);
        let actual: Vec<char> = slice_chars(document, actual_start..char_start)
            .chars()
            .collect();
        evidence += 0.25 * trailing_agreement(&before, &actual);
        terms += 1;
    }

    if !after.is_empty() {
        // 后文：紧贴标注**之后**的文字，从头往后比
        let total = document.chars().count() as u64;
        let end = (char_end + after.len() as u64).min(total);
        let actual: Vec<char> = slice_chars(document, char_end..end).chars().collect();
        evidence += 0.25 * leading_agreement(&after, &actual);
        terms += 1;
    }

    if let Some(hint) = hint {
        let doc_len = document.chars().count().max(1) as f32;
        let distance = (char_start as f32 - hint as f32).abs();
        let proximity = (1.0 - distance / doc_len).clamp(0.0, 1.0);
        evidence += 0.25 * proximity;
        terms += 1;
    }

    // 退化情形：只有"命中原文"这一项证据
    if terms == 1 {
        evidence = if occurrence_count == 1 { 0.8 } else { 0.4 };
    }

    evidence.min(1.0)
}

/// 把上下文截到 [`CONTEXT_CHARS`]，并保持贴合标注的那一侧完整。
///
/// `take_tail = true` 用于前文（保留靠近标注的结尾部分），
/// `false` 用于后文（保留靠近标注的开头部分）。
fn truncate_context(text: &str, take_tail: bool) -> Vec<char> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= CONTEXT_CHARS {
        return chars;
    }
    if take_tail {
        chars[chars.len() - CONTEXT_CHARS..].to_vec()
    } else {
        chars[..CONTEXT_CHARS].to_vec()
    }
}

/// 忽略空白差异的匹配。
///
/// 做法：把文档与 highlight 都去掉空白，在"压缩后的文档"里找，
/// 再把压缩坐标映射回原文档的字符偏移。
fn match_ignoring_whitespace(
    document: &str,
    highlight: &str,
    query: &AnchorQuery<'_>,
) -> Option<Anchored> {
    let compressed_highlight: String = highlight.chars().filter(|c| !c.is_whitespace()).collect();
    if compressed_highlight.is_empty() {
        return None;
    }

    // (压缩后第 n 个字符 → 原文档中的字符偏移)
    let mut compressed_document = String::with_capacity(document.len());
    let mut mapping: Vec<u64> = Vec::new();
    for (char_offset, character) in document.chars().enumerate() {
        if character.is_whitespace() {
            continue;
        }
        compressed_document.push(character);
        mapping.push(char_offset as u64);
    }

    let byte_start = compressed_document.find(&compressed_highlight)?;
    let start_index = compressed_document
        .get(..byte_start)
        .map(|prefix| prefix.chars().count())?;
    let end_index = start_index + compressed_highlight.chars().count();

    let char_start = *mapping.get(start_index)?;
    // 结束位置取"最后一个命中字符的原偏移 + 1"
    let char_end = mapping.get(end_index.saturating_sub(1)).copied()? + 1;

    // 忽略空白属于"能解释得通"的漂移：给一个确定性中等的分，
    // 但仍要求上下文能对上（否则可能只是碰巧同一串字）
    let confidence = 0.5 + 0.4 * context_agreement(document, char_start, char_end, query);
    let candidate = Anchored {
        range: char_start..char_end,
        confidence: confidence.min(0.95),
        whitespace_tolerant: true,
    };
    if candidate.confidence >= MIN_CONFIDENCE {
        Some(candidate)
    } else {
        None
    }
}

/// 上下文吻合度（用于空白容忍匹配）：取前后吻合比例的均值。
fn context_agreement(
    document: &str,
    char_start: u64,
    char_end: u64,
    query: &AnchorQuery<'_>,
) -> f32 {
    let normalize =
        |text: &str| -> Vec<char> { text.chars().filter(|c| !c.is_whitespace()).collect() };

    let mut parts: Vec<f32> = Vec::new();
    if let Some(before) = query.before {
        let expected = normalize(before);
        if !expected.is_empty() {
            let actual_start = char_start.saturating_sub(expected.len() as u64);
            let actual = normalize(&slice_chars(document, actual_start..char_start));
            parts.push(trailing_agreement(&expected, &actual));
        }
    }
    if let Some(after) = query.after {
        let expected = normalize(after);
        if !expected.is_empty() {
            let total = document.chars().count() as u64;
            let end = (char_end + expected.len() as u64).min(total);
            let actual = normalize(&slice_chars(document, char_end..end));
            parts.push(leading_agreement(&expected, &actual));
        }
    }
    if parts.is_empty() {
        // 完全没有上下文：只能靠"唯一命中"作为弱证据
        return if document.matches(query.highlight.trim()).count() == 1 {
            0.75
        } else {
            0.0
        };
    }
    parts.iter().sum::<f32>() / parts.len() as f32
}

fn trailing_agreement(expected: &[char], actual: &[char]) -> f32 {
    let mut matched = 0usize;
    let mut left = expected.iter().rev();
    let mut right = actual.iter().rev();
    while let (Some(a), Some(b)) = (left.next(), right.next()) {
        if a != b {
            break;
        }
        matched += 1;
    }
    matched as f32 / expected.len().max(1) as f32
}

fn leading_agreement(expected: &[char], actual: &[char]) -> f32 {
    let mut matched = 0usize;
    for (a, b) in expected.iter().zip(actual.iter()) {
        if a != b {
            break;
        }
        matched += 1;
    }
    matched as f32 / expected.len().max(1) as f32
}

/// 按**字符**偏移切片。
fn slice_chars(text: &str, range: Range<u64>) -> String {
    let start = range.start as usize;
    let end = range.end as usize;
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const DOC: &str = "第一章。天亮了，灯塔的守夜人合上书。海风很大。第二章。天亮了，灯塔的守夜人合上书。然后他睡了。";

    #[test]
    fn finds_unique_highlight_with_context() {
        let query = AnchorQuery {
            highlight: "海风很大",
            before: Some("灯塔的守夜人合上书。"),
            // 注意：after 必须是**紧贴**标注之后的原文，句号也算
            after: Some("。第二章。"),
        };
        let found = reanchor(DOC, &query).unwrap();
        assert_eq!(&slice_chars(DOC, found.range.clone()), "海风很大");
        assert!(found.confidence >= 0.9, "唯一命中 + 上下文吻合应高分");
        assert!(!found.whitespace_tolerant);
    }

    #[test]
    fn context_disambiguates_repeated_text() {
        // "灯塔的守夜人合上书" 出现两次，靠后文区分
        let query = AnchorQuery {
            highlight: "灯塔的守夜人合上书",
            before: None,
            after: Some("。然后他睡了"),
        };
        let found = reanchor(DOC, &query).unwrap();
        // 应该命中第二次出现的位置
        let text = slice_chars(DOC, found.range.clone());
        assert_eq!(text, "灯塔的守夜人合上书");
        let after = slice_chars(DOC, (found.range.end)..(found.range.end + 5));
        assert!(
            after.starts_with("。然后他"),
            "应选中后文匹配的那一处，实际后文={after}"
        );
        assert!(found.confidence > 0.7, "实际置信度={}", found.confidence);
    }

    #[test]
    fn ambiguous_without_context_is_rejected() {
        // 出现两次、又没有上下文 → 无法区分，应低于阈值而被丢弃
        let query = AnchorQuery {
            highlight: "灯塔的守夜人合上书",
            before: None,
            after: None,
        };
        assert!(
            reanchor(DOC, &query).is_none(),
            "多处命中且无上下文时宁可返回 None"
        );
    }

    #[test]
    fn unique_without_context_is_accepted() {
        let query = AnchorQuery {
            highlight: "海风很大",
            before: None,
            after: None,
        };
        let found = reanchor(DOC, &query).unwrap();
        assert!((found.confidence - 0.8).abs() < 1e-6);
    }

    #[test]
    fn tolerates_whitespace_differences() {
        // 原文里的空白被改过（重新排版的典型情况）
        let document = "第一段\n\n第二段的内容在这里。\n第三段";
        let query = AnchorQuery {
            highlight: "第二段的内容在这里。",
            before: Some("第一段"),
            after: Some("第三段"),
        };
        let found = reanchor(document, &query).unwrap();
        assert_eq!(
            &slice_chars(document, found.range.clone()),
            "第二段的内容在这里。"
        );
        assert!(found.whitespace_tolerant);
    }

    #[test]
    fn tolerates_highlight_with_different_spacing() {
        let document = "前面 hello world 后面";
        let query = AnchorQuery {
            highlight: "helloworld",
            before: Some("前面"),
            after: Some("后面"),
        };
        let found = reanchor(document, &query).unwrap();
        assert!(found.whitespace_tolerant);
        // 命中区间应覆盖原文档里的 "hello world"（含中间空格）
        let text = slice_chars(document, found.range.clone());
        assert!(text.contains("hello"), "实际命中={text:?}");
    }

    #[test]
    fn returns_none_when_highlight_absent() {
        let query = AnchorQuery {
            highlight: "完全不存在的句子",
            before: Some("第一章"),
            after: None,
        };
        assert!(reanchor(DOC, &query).is_none());
    }

    #[test]
    fn rejects_empty_highlight() {
        let query = AnchorQuery {
            highlight: "   ",
            before: None,
            after: None,
        };
        assert!(reanchor(DOC, &query).is_none());
        assert!(
            reanchor(
                "",
                &AnchorQuery {
                    highlight: "x",
                    before: None,
                    after: None
                }
            )
            .is_none()
        );
    }

    #[test]
    fn offsets_are_characters_not_bytes() {
        // 全中文：字符偏移与字节偏移完全不同，这里断言按字符算
        let document = "你好世界再见";
        let query = AnchorQuery {
            highlight: "再见",
            before: Some("你好世界"),
            after: None,
        };
        let found = reanchor(document, &query).unwrap();
        assert_eq!(found.range.start, 4, "『再』是第 5 个字符，0 基索引 4");
        assert_eq!(found.range.end, 6);
    }

    #[test]
    fn partial_context_still_scores_reasonably() {
        // 上下文只对得上一半
        let query = AnchorQuery {
            highlight: "灯塔的守夜人合上书",
            before: Some("完全不对的前文"),
            after: Some("。然后他睡了"),
        };
        let found = reanchor(DOC, &query).unwrap();
        // 前文完全不符、后文完全吻合 → 约 0.5 + 0 + 0.25
        assert!(
            found.confidence > 0.7 && found.confidence < 1.0,
            "实际置信度={}",
            found.confidence
        );
    }

    #[test]
    fn directional_agreement_helpers_behave() {
        let chars = |text: &str| text.chars().collect::<Vec<_>>();
        // 前文用"从尾往前比"：尾部对上几个算几个
        assert!((trailing_agreement(&chars("abc"), &chars("xabc")) - 1.0).abs() < 1e-6);
        assert!((trailing_agreement(&chars("abc"), &chars("abcx")) - 0.0).abs() < 1e-6);
        // 后文用"从头往后比"
        assert!((leading_agreement(&chars("abc"), &chars("abcx")) - 1.0).abs() < 1e-6);
        assert!((leading_agreement(&chars("abc"), &chars("xabc")) - 0.0).abs() < 1e-6);
        assert!((leading_agreement(&chars("abc"), &chars("ab")) - 2.0 / 3.0).abs() < 1e-6);
    }

    // ---- position hint（对齐 Hypothesis 的 TextPositionSelector 先验）----

    #[test]
    fn hint_disambiguates_repeated_text_without_context() {
        // 同一句话出现两次、没有上下文 —— 单靠 reanchor 会被丢弃
        let query = AnchorQuery {
            highlight: "灯塔的守夜人合上书",
            before: None,
            after: None,
        };
        assert!(reanchor(DOC, &query).is_none(), "无 hint 时应保守放弃");

        // 第一次出现的字符偏移是 11（"第一章。天亮了，" 之后）
        let first_at = DOC.find("灯塔的守夜人合上书").unwrap();
        let first_char = DOC[..first_at].chars().count() as u64;

        // 给出指向第一次出现的 hint → 采纳，且选中的是第一次
        let found = reanchor_with_hint(DOC, &query, Some(first_char)).expect("有 hint 时应能定位");
        assert_eq!(found.range.start, first_char);
        assert!(found.confidence >= 0.6, "置信度={}", found.confidence);

        // 指向第二次出现的 hint → 选中第二次
        let second_byte = DOC[first_at + "灯塔的守夜人合上书".len()..]
            .find("灯塔的守夜人合上书")
            .unwrap()
            + first_at
            + "灯塔的守夜人合上书".len();
        let second_char = DOC[..second_byte].chars().count() as u64;
        let found = reanchor_with_hint(DOC, &query, Some(second_char)).unwrap();
        assert_eq!(found.range.start, second_char);
    }

    #[test]
    fn context_outweighs_a_misleading_hint() {
        // 上下文明确指向第二次出现，但 hint 指向很远的开头
        let query = AnchorQuery {
            highlight: "灯塔的守夜人合上书",
            before: None,
            after: Some("。然后他睡了"),
        };
        let found = reanchor_with_hint(DOC, &query, Some(0)).unwrap();
        let after_start = found.range.end;
        let after = slice_chars(DOC, after_start..(after_start + 5));
        assert!(
            after.starts_with("。然后他"),
            "上下文应当胜过仅有 0.25 权重的 hint，实际后文={after}"
        );
    }

    #[test]
    fn long_context_is_truncated_to_the_side_touching_the_quote() {
        // 前文超过 32 字符：应保留**紧贴标注的结尾部分**，而不是开头
        let long_before = format!("{}灯塔的守夜人合上书。", "无关的废话".repeat(20));
        let document = format!("{long_before}海风很大。后文");
        let query = AnchorQuery {
            highlight: "海风很大",
            // 传完整的长前文，内部会截断
            before: Some(&long_before),
            after: Some("。后文"),
        };
        let found = reanchor(&document, &query).unwrap();
        assert_eq!(&slice_chars(&document, found.range.clone()), "海风很大");
        assert!(
            found.confidence > 0.9,
            "截断后仍应完整吻合紧贴部分，置信度={}",
            found.confidence
        );
    }

    #[test]
    fn truncate_context_keeps_the_near_side() {
        let text = "0123456789".repeat(10); // 100 字符
        let tail = truncate_context(&text, true);
        let head = truncate_context(&text, false);
        assert_eq!(tail.len(), CONTEXT_CHARS);
        assert_eq!(head.len(), CONTEXT_CHARS);
        // 前文保留结尾：最后一个是 '9'
        assert_eq!(*tail.last().unwrap(), '9');
        // 后文保留开头：第一个是 '0'
        assert_eq!(head[0], '0');
        // 短文本不截断
        assert_eq!(truncate_context("短", true).len(), 1);
    }

    #[test]
    fn slice_chars_handles_cjk_and_bounds() {
        assert_eq!(slice_chars("中文abc", 0..2), "中文");
        assert_eq!(slice_chars("中文abc", 2..5), "abc");
        // 索引：中0 文1 a2 b3 c4 —— 4..100 只剩 "c"
        assert_eq!(slice_chars("中文abc", 4..100), "c");
        assert_eq!(slice_chars("中文abc", 3..5), "bc");
    }
}
