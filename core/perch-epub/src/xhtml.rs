//! XHTML 索引：在"原始字节偏移"与"CFI"之间互相换算。
//!
//! # 它解决什么问题
//!
//! [`crate::cfi`] 能读写 CFI 字符串，但真正的锚点定位还需要一步：
//! **把内容文档里的一个位置（字节偏移）换算成 CFI，反之亦然**。
//! 本模块就是这一步，它是"M4 标注不漂移"的最后一环：
//!
//! - 生成方向：[`XhtmlIndex::locate_byte`] —— 读着书时把当前位置变成 CFI 存下来
//! - 解析方向：[`XhtmlIndex::resolve`] —— 读出 CFI 后回到原文，取出摘录
//!
//! # CFI 的文本索引规则（本模块的实现依据）
//!
//! 在一个元素作用域内，**元素子节点与文本节点各自独立编号**：
//!
//! - 第 n 个**元素**子节点 → 索引 `2n`（偶数）
//! - 第 m 个**文本**子节点 → 索引 `2m - 1`（奇数）
//!
//! 所以 `<body><p>甲</p><p>乙</p></body>` 里两个 `<p>` 是 `/2`、`/4`，
//! 而 `甲` 是 `/2/1`、`乙` 是 `/4/1`。
//!
//! # 三个容易踩的坑（都已处理）
//!
//! 1. **必须保留空白文本节点**：`reader.trim_text(false)`。若把换行缩进丢掉，
//!    文本索引会整体错位，算出的 CFI 与 Readium 对不上。
//! 2. **实体引用要合并进同一个文本节点**：quick-xml 会把 `&amp;` 单独发成
//!    [`Event::GeneralRef`]，但 DOM 语义上 `甲&amp;乙` 是**一个**文本节点。
//!    因此本模块把相邻的 Text / CData / GeneralRef 事件合并后再落盘。
//! 3. **`:n` 是字符偏移，不是字节偏移**：中文一个字占 3 字节，按字节算会错得很离谱。
//!    本模块在切前缀时会先吸附到字符边界（`is_char_boundary`），再数字符。
//!
//! # 已知取舍
//!
//! - 偏移若正好落在实体引用内部（如 `&am` 中间），字符偏移按原样计数而不报错——
//!   宁可给一个略偏的值，也不要让整次定位失败。
//! - 只索引文本节点；注释、处理指令不占文本索引（符合规范）。

use std::ops::Range;

use quick_xml::events::{BytesStart, Event};
use thiserror::Error;

use crate::cfi::{CfiPath, Step};
use crate::xml;

/// XHTML 索引构建错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum XhtmlError {
    /// 输入不是合法 UTF-8。
    #[error("XHTML 不是合法的 UTF-8")]
    NotUtf8,

    /// XML 语法错误。
    #[error("XML 解析失败: {0}")]
    Xml(String),
}

impl XhtmlError {
    /// 稳定的错误码。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotUtf8 => "XHTML_NOT_UTF8",
            Self::Xml(_) => "XHTML_XML",
        }
    }
}

/// 一个文本节点及其 CFI 定位信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextNode {
    /// 指向该文本节点的 CFI 路径（末段为奇数索引的字符步，`text_offset` 为空）。
    pub path: CfiPath,
    /// 该文本在原始 XHTML 字节中的范围。
    pub range: Range<usize>,
    /// 实体反转义之后的文本。
    pub text: String,
}

/// 一次定位的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located<'a> {
    /// 完整的 CFI 路径（`text_offset` 已填好）。
    pub path: CfiPath,
    /// 在该文本节点内的**字符**偏移。
    pub char_offset: u32,
    /// 命中文本节点的全文——方便直接取摘录（excerpt）。
    pub text: &'a str,
}

/// 一个元素作用域的子节点计数。
#[derive(Debug, Clone, Copy, Default)]
struct Scope {
    /// 已见元素子节点数。
    elements: u32,
    /// 已见文本子节点数。
    texts: u32,
}

/// 正在累积的文本节点（相邻的 Text / CData / GeneralRef 会合并进它）。
#[derive(Debug)]
struct PendingText {
    start: usize,
    end: usize,
    decoded: String,
}

impl PendingText {
    fn new(start: usize) -> Self {
        Self {
            start,
            end: start,
            decoded: String::new(),
        }
    }

    fn push(&mut self, text: &str, end: usize) {
        self.decoded.push_str(text);
        self.end = end.max(self.start);
    }
}

/// 一个内容文档（XHTML）的定位索引。
pub struct XhtmlIndex {
    /// 原始 XHTML 文本，用于按字节偏移切前缀。
    source: String,
    /// 按文档顺序排列的文本节点。
    nodes: Vec<TextNode>,
}

impl XhtmlIndex {
    /// 遍历 XHTML，建立文本节点索引。
    ///
    /// # Errors
    ///
    /// - 不是合法 UTF-8 → [`XhtmlError::NotUtf8`]
    /// - XML 语法错误 → [`XhtmlError::Xml`]
    pub fn build(xhtml: &[u8]) -> Result<Self, XhtmlError> {
        let source = std::str::from_utf8(xhtml)
            .map_err(|_| XhtmlError::NotUtf8)?
            .to_owned();

        let mut reader = quick_xml::Reader::from_str(&source);
        // 关键：保留空白文本节点，否则文本索引会整体错位（见模块文档）
        reader.config_mut().trim_text(false);

        let mut path: Vec<Step> = Vec::new();
        // 栈底永远是文档作用域（`<html>` 是它的第 1 个元素子节点 → 索引 2）
        let mut scopes: Vec<Scope> = vec![Scope::default()];
        let mut nodes: Vec<TextNode> = Vec::new();
        let mut pending: Option<PendingText> = None;

        loop {
            let before = reader.buffer_position() as usize;
            let event = reader
                .read_event()
                .map_err(|err| XhtmlError::Xml(err.to_string()))?;
            let after = reader.buffer_position() as usize;

            // 文本类事件：累积进 pending，不落盘（要等相邻文本全部到齐）
            match &event {
                Event::Text(_) | Event::GeneralRef(_) => {
                    let raw = slice_at(&source, before, after);
                    let decoded = decode(raw);
                    pending
                        .get_or_insert_with(|| PendingText::new(before))
                        .push(&decoded, after);
                    continue;
                }
                Event::CData(_) => {
                    let raw = slice_at(&source, before, after);
                    // 范围只覆盖 CDATA 的内容，不含 `<![CDATA[`（9 字节）与 `]]>`（3 字节）
                    let (start, end, inner) = match strip_cdata_markers(raw) {
                        Some(inner) => (
                            before + CDATA_PREFIX.len(),
                            after.saturating_sub(CDATA_SUFFIX.len()),
                            inner,
                        ),
                        // 理论上不可达（quick-xml 保证 CDATA 事件带标记）；退化处理不 panic
                        None => (before, after, raw),
                    };
                    pending
                        .get_or_insert_with(|| PendingText::new(start))
                        .push(inner, end);
                    continue;
                }
                _ => {}
            }

            // 其它事件意味着文本节点结束：先落盘
            if let Some(text) = pending.take() {
                let index = take_text_index(&mut scopes);
                let mut steps = path.clone();
                steps.push(Step::new(index, None));
                nodes.push(TextNode {
                    path: CfiPath::new(Vec::new(), steps, None),
                    range: text.start..text.end,
                    text: text.decoded,
                });
            }

            match event {
                Event::Start(element) => {
                    path.push(take_element_step(&mut scopes, &element));
                    scopes.push(Scope::default());
                }
                Event::Empty(element) => {
                    // 自闭合元素也是元素子节点，占一个索引，但没有子作用域
                    let _ = take_element_step(&mut scopes, &element);
                }
                Event::End(_) => {
                    path.pop();
                    if scopes.len() > 1 {
                        scopes.pop();
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }

        Ok(Self { source, nodes })
    }

    /// 按文档顺序的全部文本节点。
    #[must_use]
    pub fn text_nodes(&self) -> &[TextNode] {
        &self.nodes
    }

    /// 原始 XHTML 文本。
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// 是否没有任何文本节点。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 把**原始字节偏移**定位成 CFI。
    ///
    /// 返回 `None` 表示该偏移不落在任何文本节点内。
    #[must_use]
    pub fn locate_byte(&self, byte_offset: usize) -> Option<Located<'_>> {
        let node = self
            .nodes
            .iter()
            .find(|node| node.range.contains(&byte_offset))
            // 偏移正好落在某个文本节点末尾时（例如选区结束于节点边界），归属前一个节点
            .or_else(|| {
                self.nodes
                    .iter()
                    .rev()
                    .find(|node| node.range.end == byte_offset)
            })?;

        let char_offset = self.char_offset(node, byte_offset);
        let mut path = node.path.clone();
        path.text_offset = Some(char_offset);
        Some(Located {
            path,
            char_offset,
            text: &node.text,
        })
    }

    /// 由 CFI 路径找到对应文本节点。
    ///
    /// 匹配分两轮：先比较整段路径（含断言），再退化为**只比较索引**
    /// —— 因为标注来源的 CFI 可能带 id 断言而我们没有，反之亦然。
    #[must_use]
    pub fn resolve(&self, path: &CfiPath) -> Option<&TextNode> {
        let local = path.local.as_slice();
        self.nodes
            .iter()
            .find(|node| node.path.local.as_slice() == local)
            .or_else(|| {
                self.nodes
                    .iter()
                    .find(|node| same_step_indices(&node.path.local, local))
            })
    }

    /// 由 CFI 路径回到原始字节偏移。
    #[must_use]
    pub fn byte_offset_of(&self, path: &CfiPath) -> Option<usize> {
        self.resolve(path).map(|node| node.range.start)
    }

    /// 计算字节偏移在文本节点内的**字符**偏移。
    fn char_offset(&self, node: &TextNode, byte_offset: usize) -> u32 {
        let mut end = byte_offset.clamp(node.range.start, node.range.end);
        // 先吸附到字符边界：偏移落在多字节字符中间时不能直接切
        while end > node.range.start && !self.source.is_char_boundary(end) {
            end -= 1;
        }
        let prefix = self.source.get(node.range.start..end).unwrap_or("");
        match quick_xml::escape::unescape(prefix) {
            Ok(decoded) => decoded.chars().count() as u32,
            // 偏移落在实体引用内部：退化为按原样计数，不让定位失败
            Err(_) => prefix.chars().count() as u32,
        }
    }
}

impl std::fmt::Debug for XhtmlIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XhtmlIndex")
            .field("text_nodes", &self.nodes.len())
            .field("source_len", &self.source.len())
            .finish()
    }
}

/// 安全切片（越界或非字符边界时退化为空串）。
fn slice_at(source: &str, start: usize, end: usize) -> &str {
    source.get(start..end).unwrap_or("")
}

/// CDATA 起始标记。
const CDATA_PREFIX: &str = "<![CDATA[";
/// CDATA 结束标记。
const CDATA_SUFFIX: &str = "]]>";

/// 去掉 CDATA 标记，取出内容。
fn strip_cdata_markers(raw: &str) -> Option<&str> {
    raw.strip_prefix(CDATA_PREFIX)
        .and_then(|body| body.strip_suffix(CDATA_SUFFIX))
}

/// 实体反转义；失败时退化为原文（不让个别畸形实体毁掉整次索引）。
fn decode(raw: &str) -> String {
    quick_xml::escape::unescape(raw)
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| raw.to_owned())
}

/// 占用下一个元素子节点索引（偶数）。
fn take_element_step(scopes: &mut [Scope], element: &BytesStart<'_>) -> Step {
    let index = match scopes.last_mut() {
        Some(scope) => {
            scope.elements += 1;
            scope.elements * 2
        }
        // 不可达：栈底永远是文档作用域。退化处理而不是 panic。
        None => 2,
    };
    Step::new(index, xml::attr(element, "id"))
}

/// 占用下一个文本子节点索引（奇数）。
fn take_text_index(scopes: &mut [Scope]) -> u32 {
    match scopes.last_mut() {
        Some(scope) => {
            scope.texts += 1;
            scope.texts * 2 - 1
        }
        // 不可达
        None => 1,
    }
}

/// 只比较路径段的索引，忽略断言。
fn same_step_indices(left: &[Step], right: &[Step]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.index == right.index)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// 刻意写成一行：避免多余空白文本节点干扰断言。
    const XHTML: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>标题</title></head><body><h1 id="c1">第一章</h1><p>第一段。</p><p>第二段 &amp; 结尾</p></body></html>"#;

    fn index() -> XhtmlIndex {
        XhtmlIndex::build(XHTML.as_bytes()).unwrap()
    }

    fn paths(index: &XhtmlIndex) -> Vec<String> {
        index
            .text_nodes()
            .iter()
            .map(|node| node.path.to_string())
            .collect()
    }

    #[test]
    fn indexes_text_nodes_in_document_order() {
        let index = index();
        assert_eq!(
            paths(&index),
            vec!["/2/2/2/1", "/2/4/2[c1]/1", "/2/4/4/1", "/2/4/6/1"]
        );
        assert_eq!(
            index
                .text_nodes()
                .iter()
                .map(|node| node.text.as_str())
                .collect::<Vec<_>>(),
            vec!["标题", "第一章", "第一段。", "第二段 & 结尾"]
        );
    }

    #[test]
    fn element_steps_are_even_and_text_steps_odd() {
        let index = index();
        for node in index.text_nodes() {
            // 末段必须是奇数（字符节点）
            assert!(
                node.path.local.last().unwrap().is_character(),
                "路径 {node:?}"
            );
            // 前导段必须是元素（偶数）
            for step in &node.path.local[..node.path.local.len() - 1] {
                assert!(step.is_element(), "非法的元素段: {step:?}");
            }
            // 整体也要通过 CFI 的奇偶校验
            node.path.validate_indices().unwrap();
        }
    }

    #[test]
    fn takes_id_attribute_as_assertion() {
        let index = index();
        let node = index
            .text_nodes()
            .iter()
            .find(|node| node.text == "第一章")
            .unwrap();
        // /2/4/2[id=c1]/1
        assert_eq!(node.path.local[2].index, 2);
        assert_eq!(node.path.local[2].assertion.as_deref(), Some("c1"));
    }

    #[test]
    fn entities_are_decoded_into_one_text_node() {
        let index = index();
        let node = index
            .text_nodes()
            .iter()
            .find(|node| node.text.contains('&'))
            .unwrap();
        // 关键：&amp; 不能被拆成三个文本节点，decode 后应是单个 '&'
        assert_eq!(node.text, "第二段 & 结尾");
    }

    #[test]
    fn locate_byte_returns_path_and_char_offset() {
        let index = index();
        let base = XHTML.find("第一段。").unwrap();

        // 段首
        let located = index.locate_byte(base).unwrap();
        assert_eq!(located.path.to_string(), "/2/4/4/1:0");
        assert_eq!(located.char_offset, 0);
        assert_eq!(located.text, "第一段。");

        // 前进 3 字节 = 前进 1 个汉字
        let located = index.locate_byte(base + 3).unwrap();
        assert_eq!(located.path.to_string(), "/2/4/4/1:1");
        assert_eq!(located.char_offset, 1);

        // 整个文本节点末尾
        let located = index.locate_byte(base + "第一段。".len()).unwrap();
        assert_eq!(located.char_offset, 4);
    }

    #[test]
    fn locate_byte_snaps_to_char_boundary() {
        let index = index();
        let base = XHTML.find("第一段。").unwrap();
        // 落在"第"字的中间（UTF-8 三字节，取第二字节）
        let located = index.locate_byte(base + 1).unwrap();
        assert_eq!(located.char_offset, 0, "应吸附回字符边界");
    }

    #[test]
    fn locate_byte_handles_positions_outside_text() {
        let index = index();
        // 标签内的字节偏移不落在任何文本节点上
        assert!(index.locate_byte(0).is_none());
        // 超出末尾也不是
        assert!(index.locate_byte(XHTML.len() + 10).is_none());
    }

    #[test]
    fn resolve_finds_text_node_by_path() {
        let index = index();
        let path = CfiPath::parse("/2/4/4/1").unwrap();
        assert_eq!(index.resolve(&path).unwrap().text, "第一段。");
    }

    #[test]
    fn resolve_ignores_assertions_and_text_offset() {
        let index = index();
        // 带字符偏移
        let with_offset = CfiPath::parse("/2/4/4/1:2").unwrap();
        assert!(index.resolve(&with_offset).is_some());
        // 带断言（我们的索引里 h1 才有 id，这里给 title 的路径加上断言）
        let with_assertion = CfiPath::parse("/2/2/2[title]/1").unwrap();
        assert_eq!(index.resolve(&with_assertion).unwrap().text, "标题");
        // 未知路径
        assert!(
            index
                .resolve(&CfiPath::parse("/2/4/8/1").unwrap())
                .is_none()
        );
    }

    #[test]
    fn byte_offset_round_trips_for_every_node() {
        let index = index();
        for node in index.text_nodes() {
            let offset = index.byte_offset_of(&node.path).unwrap();
            assert_eq!(offset, node.range.start);
            let located = index.locate_byte(offset).unwrap();
            assert_eq!(located.path.text_offset, Some(0));
            assert_eq!(located.text, node.text);
            // 用定位结果再解析，应回到同一个节点
            assert_eq!(index.resolve(&located.path).unwrap().text, node.text);
        }
    }

    #[test]
    fn whitespace_only_text_nodes_are_indexed() {
        // 这条测试把"保留空白文本节点"这个决定固定下来：
        // 丢掉空白会让文本索引整体错位，算出的 CFI 与 Readium 对不上。
        let pretty = "<html>\n  <body>\n    <p>甲</p>\n  </body>\n</html>";
        let index = XhtmlIndex::build(pretty.as_bytes()).unwrap();
        assert_eq!(
            paths(&index),
            vec!["/2/1", "/2/2/1", "/2/2/2/1", "/2/2/3", "/2/3"]
        );
        assert_eq!(
            index
                .text_nodes()
                .iter()
                .map(|node| node.text.as_str())
                .collect::<Vec<_>>(),
            vec!["\n  ", "\n    ", "甲", "\n  ", "\n"]
        );
    }

    #[test]
    fn empty_document_has_no_text_nodes() {
        let index = XhtmlIndex::build(b"<html></html>").unwrap();
        assert!(index.is_empty());
        assert!(index.text_nodes().is_empty());
    }

    #[test]
    fn rejects_non_utf8_input() {
        let error = XhtmlIndex::build(&[0xff, 0xfe, 0x41]).unwrap_err();
        assert_eq!(error.code(), "XHTML_NOT_UTF8");
    }

    #[test]
    fn rejects_malformed_xml() {
        let error = XhtmlIndex::build(b"<html><body></html>").unwrap_err();
        assert_eq!(error.code(), "XHTML_XML");
    }

    #[test]
    fn cdata_content_is_indexed_without_markers() {
        let index = XhtmlIndex::build(b"<html><body><p><![CDATA[a&b]]></p></body></html>").unwrap();
        let node = &index.text_nodes()[0];
        assert_eq!(node.text, "a&b");
        // 范围只覆盖 CDATA 内容，不含 `<![CDATA[` 与 `]]>`
        assert_eq!(&index.source()[node.range.clone()], "a&b");
    }

    #[test]
    fn nested_elements_get_nested_paths() {
        let index =
            XhtmlIndex::build(b"<html><body><div><p><em>x</em></p></div></body></html>").unwrap();
        // html=2, body=2, div=2, p=2, em=2, 文本=1
        assert_eq!(paths(&index), vec!["/2/2/2/2/2/1"]);
    }
}
