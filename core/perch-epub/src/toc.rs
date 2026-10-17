//! 目录（TOC）解析：EPUB3 的 NAV 文档与 EPUB2 的 NCX。
//!
//! # 两种目录格式
//!
//! - **EPUB 3**：`nav.xhtml` 里一个 `<nav epub:type="toc">`，内部是嵌套的 `<ol>/<li>/<a>`
//! - **EPUB 2**：`toc.ncx` 里一个 `<navMap>`，内部是嵌套的 `<navPoint>`
//!
//! 两者描述的是同一件事（章节目录树），所以本模块把它们统一成同一种 [`TocEntry`] 树，
//! 上层不必关心书是 EPUB 2 还是 3 —— 由 [`Package::nav_item`](crate::Package::nav_item)
//! 和 [`Package::ncx_item`](crate::Package::ncx_item) 决定该用哪个。
//!
//! # 实现方式：两遍法
//!
//! 第一遍先定位目标元素（`<nav epub:type="toc">` 或 `<navMap>`）的**字节范围**，
//! 第二遍只对这段范围跑状态机。好处是目录构建的状态机不必关心 nav 的嵌套层级、
//! 也不必判断"当前是不是在正确的 nav 里"，逻辑能少一半。
//!
//! # 树是怎么搭起来的
//!
//! 用一个"层级栈"：遇到容器（`<ol>` / `<navPoint>`）就压一层，
//! 遇到条目结束（`</li>` / `</navPoint>`）就把刚建好的条目挂到**上一层**。
//! 这样任意深度的嵌套都自然成立，不需要递归。
//!
//! # 已知行为
//!
//! **截断的 XML（标签未闭合到文件末尾）不会报错**，会得到"部分目录"——因为 quick-xml
//! 只对*结束标签不匹配*报错，不对*未闭合到 EOF* 报错。这是有意的宽容：目录残缺不该让
//! 整本书打不开。真正的语法错误（标签不匹配）仍会返回 [`EpubError::Xml`]。

use std::ops::Range;

use quick_xml::events::{BytesStart, Event};

use crate::error::EpubError;
use crate::package::resolve_href;
use crate::xml;

/// 目录条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocEntry {
    /// 标题。若目录里没有可见文字（如只有图标），退化为 `href`。
    pub title: String,
    /// 目标位置：**容器绝对路径**，可能带 `#片段`。
    ///
    /// 分组用的条目（只有子目录、自己不是链接）会是空串。
    pub href: String,
    /// 子条目。
    pub children: Vec<TocEntry>,
}

impl TocEntry {
    /// 构造一个叶子条目。
    #[must_use]
    pub fn new(title: impl Into<String>, href: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            href: href.into(),
            children: Vec::new(),
        }
    }

    /// 是否为分组条目（自己不是链接）。
    #[must_use]
    pub fn is_group(&self) -> bool {
        self.href.is_empty()
    }

    /// 深度优先展平（含自身与全部后代，按显示顺序）。
    ///
    /// 列表 UI 与"跳到第 n 项"都用它。
    #[must_use]
    pub fn flatten(&self) -> Vec<&Self> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect<'a>(&'a self, out: &mut Vec<&'a Self>) {
        out.push(self);
        for child in &self.children {
            child.collect(out);
        }
    }

    /// 子树的层数（叶子为 1）。
    #[must_use]
    pub fn subtree_depth(&self) -> usize {
        1 + self
            .children
            .iter()
            .map(Self::subtree_depth)
            .max()
            .unwrap_or(0)
    }
}

/// 深度优先展平整个目录。
#[must_use]
pub fn flatten_toc(entries: &[TocEntry]) -> Vec<&TocEntry> {
    entries.iter().flat_map(TocEntry::flatten).collect()
}

/// 解析 EPUB3 的 NAV 文档。
///
/// `base_dir` 应当是**NAV 文档所在目录**的容器路径
/// （用 [`container_dir_of`](crate::package::container_dir_of) 由条目名算出），
/// 目录里的相对 `href` 会基于它解析。
///
/// 找不到目录时返回**空列表而不是错误**：一本没有目录的书仍然是能读的，
/// 不该因为目录缺失就打不开。
///
/// # Errors
///
/// XML 语法错误 → [`EpubError::Xml`]。
pub fn parse_nav(xhtml: &[u8], base_dir: &str) -> Result<Vec<TocEntry>, EpubError> {
    let source = std::str::from_utf8(xhtml).map_err(|err| EpubError::Xml(err.to_string()))?;

    // 第一遍：优先找声明了 epub:type="toc" 的 nav；没有就退回第一个 nav
    let span = match element_span(source, |element| {
        xml::name_of(element) == "nav" && has_toc_type(element)
    })? {
        Some(span) => Some(span),
        None => element_span(source, |element| xml::name_of(element) == "nav")?,
    };
    let Some(span) = span else {
        return Ok(Vec::new());
    };
    let slice = source.get(span).unwrap_or("");

    // 第二遍：在 nav 内部构建目录树
    let mut reader = quick_xml::Reader::from_str(slice);
    reader.config_mut().trim_text(true);

    let mut levels: Vec<Vec<TocEntry>> = Vec::new();
    let mut open_items: Vec<ItemState> = Vec::new();
    let mut result: Vec<TocEntry> = Vec::new();
    let mut in_anchor = false;

    loop {
        let before = reader.buffer_position() as usize;
        let event = reader
            .read_event()
            .map_err(|err| EpubError::Xml(err.to_string()))?;
        let after = reader.buffer_position() as usize;

        match &event {
            Event::Start(element) => match xml::name_of(element) {
                "ol" => levels.push(Vec::new()),
                // 记下所属目录层级：用来判断某段文字是不是本条目自己的标题
                "li" => open_items.push(ItemState {
                    ol_depth: levels.len(),
                    ..ItemState::default()
                }),
                "a" => {
                    in_anchor = true;
                    if let Some(item) = open_items.last_mut() {
                        item.href = xml::attr(element, "href");
                    }
                }
                _ => {}
            },
            Event::Empty(element) => {
                if xml::name_of(element) == "a" {
                    if let Some(item) = open_items.last_mut() {
                        item.href = xml::attr(element, "href");
                    }
                }
            }
            Event::Text(_) | Event::GeneralRef(_) => {
                if let Some(item) = open_items.last_mut() {
                    // 收标题的两种情形：在 <a> 里；或在本条目自己的层级上（如 <span>标签</span>）
                    let in_label_zone = levels.len() == item.ol_depth;
                    if in_anchor || in_label_zone {
                        item.title
                            .push_str(&xml::decode_text(slice_at(slice, before, after)));
                    }
                }
            }
            Event::End(element) => match xml::end_name(element) {
                "a" => in_anchor = false,
                "li" => {
                    if let Some(item) = open_items.pop() {
                        if let Some(entry) = item.into_entry(base_dir) {
                            if let Some(level) = levels.last_mut() {
                                level.push(entry);
                            }
                        }
                    }
                }
                "ol" => {
                    if let Some(done) = levels.pop() {
                        match open_items.last_mut() {
                            // 嵌套目录属于外层那个 <li>
                            Some(item) => item.children = done,
                            // 最外层目录就是结果
                            None => result = done,
                        }
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }

    if result.is_empty() {
        // 兜底：XML 不太规范导致 </ol> 没有把结果交出来时，取最外层剩下的那一层
        result = levels.into_iter().next().unwrap_or_default();
    }
    Ok(result)
}

/// 解析 EPUB2 的 NCX 文档。
///
/// 语义与 [`parse_nav`] 一致，只是元素名不同（`navMap` / `navPoint` / `navLabel` / `content`）。
///
/// # Errors
///
/// XML 语法错误 → [`EpubError::Xml`]。
pub fn parse_ncx(ncx_xml: &[u8], base_dir: &str) -> Result<Vec<TocEntry>, EpubError> {
    let source = std::str::from_utf8(ncx_xml).map_err(|err| EpubError::Xml(err.to_string()))?;

    let Some(span) = element_span(source, |element| xml::name_of(element) == "navMap")? else {
        return Ok(Vec::new());
    };
    let slice = source.get(span).unwrap_or("");

    let mut reader = quick_xml::Reader::from_str(slice);
    reader.config_mut().trim_text(true);

    let mut levels: Vec<Vec<TocEntry>> = Vec::new();
    let mut open_items: Vec<ItemState> = Vec::new();
    let mut result: Vec<TocEntry> = Vec::new();
    let mut in_label_text = false;

    loop {
        let before = reader.buffer_position() as usize;
        let event = reader
            .read_event()
            .map_err(|err| EpubError::Xml(err.to_string()))?;
        let after = reader.buffer_position() as usize;

        match &event {
            Event::Start(element) => match xml::name_of(element) {
                // 根层级也要压栈，否则第一层 navPoint 建好的条目无处可挂
                "navMap" => levels.push(Vec::new()),
                "navPoint" => {
                    levels.push(Vec::new());
                    open_items.push(ItemState::default());
                }
                "text" => {
                    in_label_text = true;
                }
                "content" => {
                    if let Some(item) = open_items.last_mut() {
                        item.href = xml::attr(element, "src");
                    }
                }
                _ => {}
            },
            Event::Empty(element) => {
                if xml::name_of(element) == "content" {
                    if let Some(item) = open_items.last_mut() {
                        item.href = xml::attr(element, "src");
                    }
                }
            }
            Event::Text(_) | Event::GeneralRef(_) => {
                if in_label_text {
                    if let Some(item) = open_items.last_mut() {
                        item.title
                            .push_str(&xml::decode_text(slice_at(slice, before, after)));
                    }
                }
            }
            Event::End(element) => match xml::end_name(element) {
                "text" => in_label_text = false,
                "navPoint" => {
                    // 先收下这一层的子目录，再建条目并挂到上一层
                    let children = levels.pop().unwrap_or_default();
                    if let Some(mut item) = open_items.pop() {
                        item.children = children;
                        if let Some(entry) = item.into_entry(base_dir) {
                            if let Some(level) = levels.last_mut() {
                                level.push(entry);
                            }
                        }
                    }
                }
                "navMap" => {
                    if let Some(done) = levels.pop() {
                        result = done;
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }

    if result.is_empty() {
        result = levels.into_iter().next().unwrap_or_default();
    }
    Ok(result)
}

/// 正在构建中的条目。
#[derive(Debug, Default)]
struct ItemState {
    title: String,
    href: Option<String>,
    children: Vec<TocEntry>,
    /// 打开本条目时所在的目录层级（仅 NAV 用；NCX 有独立的 navLabel 不需要它）
    ol_depth: usize,
}

impl ItemState {
    /// 收尾成条目；标题与 href 都为空时返回 `None`（纯垃圾节点，丢掉）。
    fn into_entry(self, base_dir: &str) -> Option<TocEntry> {
        let raw_href = self.href.unwrap_or_default();
        let href = if raw_href.trim().is_empty() {
            String::new()
        } else {
            resolve_href(base_dir, raw_href.trim())
        };
        let title = self.title.trim().to_owned();
        if title.is_empty() && href.is_empty() {
            return None;
        }
        Some(TocEntry {
            // 目录里没有可见文字时用 href 兜底，总好过在书里显示一个空行
            title: if title.is_empty() {
                href.clone()
            } else {
                title
            },
            href,
            children: self.children,
        })
    }
}

/// `nav` 元素是否声明了 `epub:type` 含 `toc`。
fn has_toc_type(element: &BytesStart<'_>) -> bool {
    xml::attr(element, "type").is_some_and(|value| {
        value
            .split_whitespace()
            .any(|token| token.eq_ignore_ascii_case("toc"))
    })
}

/// 找出满足条件的元素的字节范围（含其全部子元素与标签本身）。
///
/// 用一个深度计数器配平，因此不需要理解文档结构。
fn element_span(
    source: &str,
    matches_target: impl Fn(&BytesStart<'_>) -> bool,
) -> Result<Option<Range<usize>>, EpubError> {
    let mut reader = quick_xml::Reader::from_str(source);
    reader.config_mut().trim_text(true);

    let mut target_depth: Option<usize> = None;
    let mut depth: usize = 0;
    let mut start_at: usize = 0;

    loop {
        let before = reader.buffer_position() as usize;
        let event = reader
            .read_event()
            .map_err(|err| EpubError::Xml(err.to_string()))?;
        let after = reader.buffer_position() as usize;

        match event {
            Event::Start(element) => {
                depth += 1;
                if target_depth.is_none() && matches_target(&element) {
                    target_depth = Some(depth);
                    start_at = before;
                }
            }
            Event::Empty(element) => {
                // 自闭合元素没有内容，命中即返回
                if target_depth.is_none() && matches_target(&element) {
                    return Ok(Some(before..after));
                }
            }
            Event::End(_) => {
                if let Some(target) = target_depth {
                    if depth == target {
                        return Ok(Some(start_at..after));
                    }
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
}

/// 安全切片。
fn slice_at(source: &str, start: usize, end: usize) -> &str {
    source.get(start..end).unwrap_or("")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::test_fixture;

    #[test]
    fn parses_nav_from_fixture() {
        let toc = parse_nav(test_fixture::NAV.as_bytes(), "/OEBPS").unwrap();
        assert_eq!(toc.len(), 2);

        assert_eq!(toc[0].title, "第一章");
        assert_eq!(toc[0].href, "/OEBPS/text/chap01.xhtml");
        assert!(toc[0].children.is_empty());

        assert_eq!(toc[1].title, "第二章");
        assert_eq!(toc[1].href, "/OEBPS/text/chap02.xhtml");
        assert_eq!(toc[1].children.len(), 1);
        assert_eq!(toc[1].children[0].title, "第一节");
        // 片段必须保留
        assert_eq!(toc[1].children[0].href, "/OEBPS/text/chap02.xhtml#s1");
    }

    #[test]
    fn parses_ncx_from_fixture() {
        let toc = parse_ncx(test_fixture::NCX.as_bytes(), "/OEBPS").unwrap();
        assert_eq!(toc.len(), 2);
        assert_eq!(toc[0].title, "第一章");
        assert_eq!(toc[1].children.len(), 1);
        assert_eq!(toc[1].children[0].href, "/OEBPS/text/chap02.xhtml#s1");
    }

    #[test]
    fn nav_and_ncx_describe_the_same_tree() {
        // 夹具里两种格式写的是同一份目录 —— 解析结果必须一致，
        // 否则说明有一边理解错了
        let from_nav = parse_nav(test_fixture::NAV.as_bytes(), "/OEBPS").unwrap();
        let from_ncx = parse_ncx(test_fixture::NCX.as_bytes(), "/OEBPS").unwrap();
        assert_eq!(from_nav, from_ncx);
    }

    #[test]
    fn flatten_is_depth_first() {
        let toc = parse_nav(test_fixture::NAV.as_bytes(), "/OEBPS").unwrap();
        let flat: Vec<&str> = flatten_toc(&toc)
            .iter()
            .map(|entry| entry.title.as_str())
            .collect();
        assert_eq!(flat, vec!["第一章", "第二章", "第一节"]);

        // 单条目的展平
        let single = TocEntry::new("甲", "/a.xhtml");
        assert_eq!(single.flatten().len(), 1);
    }

    #[test]
    fn subtree_depth_counts_levels() {
        let toc = parse_nav(test_fixture::NAV.as_bytes(), "/OEBPS").unwrap();
        assert_eq!(toc[0].subtree_depth(), 1);
        assert_eq!(toc[1].subtree_depth(), 2);
        // 整棵树的深度取最大值
        assert_eq!(toc.iter().map(TocEntry::subtree_depth).max(), Some(2));
    }

    #[test]
    fn nav_type_match_is_case_insensitive_and_multi_valued() {
        let xhtml = r#"<html><body>
          <nav epub:type="landmarks"><ol><li><a href="cover.xhtml">封面</a></li></ol></nav>
          <nav epub:type="TOC page-list"><ol><li><a href="c1.xhtml">正文</a></li></ol></nav>
        </body></html>"#;
        let toc = parse_nav(xhtml.as_bytes(), "/").unwrap();
        assert_eq!(toc.len(), 1);
        assert_eq!(toc[0].title, "正文", "应挑出 TOC 那个 nav 而不是第一个");
    }

    #[test]
    fn falls_back_to_first_nav_when_no_toc_type() {
        let xhtml =
            r#"<html><body><nav><ol><li><a href="c1.xhtml">甲</a></li></ol></nav></body></html>"#;
        let toc = parse_nav(xhtml.as_bytes(), "/OEBPS").unwrap();
        assert_eq!(toc.len(), 1);
        assert_eq!(toc[0].title, "甲");
    }

    #[test]
    fn missing_toc_returns_empty_not_error() {
        // 没有 nav
        assert!(
            parse_nav(b"<html><body><p>x</p></body></html>", "/")
                .unwrap()
                .is_empty()
        );
        // 没有 navMap
        assert!(parse_ncx(b"<ncx><head/></ncx>", "/").unwrap().is_empty());
    }

    #[test]
    fn grouping_entry_without_link_is_kept() {
        let xhtml = r#"<html><body><nav epub:type="toc"><ol>
          <li><span>第一部分</span><ol><li><a href="c1.xhtml">甲</a></li></ol></li>
        </ol></nav></body></html>"#;
        let toc = parse_nav(xhtml.as_bytes(), "/OEBPS").unwrap();
        assert_eq!(toc.len(), 1);
        assert!(toc[0].is_group(), "没有链接的条目应识别为分组");
        assert_eq!(toc[0].title, "第一部分");
        assert_eq!(toc[0].children.len(), 1);
        assert_eq!(toc[0].children[0].href, "/OEBPS/c1.xhtml");
    }

    #[test]
    fn anchor_without_text_falls_back_to_href() {
        let xhtml = r#"<html><body><nav epub:type="toc"><ol>
          <li><a href="c1.xhtml"/></li>
        </ol></nav></body></html>"#;
        let toc = parse_nav(xhtml.as_bytes(), "/OEBPS").unwrap();
        assert_eq!(toc.len(), 1);
        assert_eq!(toc[0].title, "/OEBPS/c1.xhtml", "无标题时用 href 兜底");
    }

    #[test]
    fn entity_in_title_is_decoded() {
        let xhtml = r#"<html><body><nav epub:type="toc"><ol>
          <li><a href="c1.xhtml">上 &amp; 下</a></li>
        </ol></nav></body></html>"#;
        let toc = parse_nav(xhtml.as_bytes(), "/").unwrap();
        assert_eq!(toc[0].title, "上 & 下");

        let ncx = r#"<ncx><navMap><navPoint><navLabel><text>甲&amp;乙</text></navLabel><content src="c.xhtml"/></navPoint></navMap></ncx>"#;
        let toc = parse_ncx(ncx.as_bytes(), "/").unwrap();
        assert_eq!(toc[0].title, "甲&乙");
    }

    #[test]
    fn deeply_nested_toc_is_built_correctly() {
        let xhtml = r#"<html><body><nav epub:type="toc"><ol>
          <li><a href="a.xhtml">一</a>
            <ol><li><a href="b.xhtml">一之一</a>
              <ol><li><a href="c.xhtml">一之一之一</a></li></ol>
            </li></ol>
          </li>
          <li><a href="d.xhtml">二</a></li>
        </ol></nav></body></html>"#;
        let toc = parse_nav(xhtml.as_bytes(), "/").unwrap();
        assert_eq!(toc.len(), 2);
        assert_eq!(toc[0].subtree_depth(), 3);
        assert_eq!(toc[0].children[0].children[0].title, "一之一之一");
        assert_eq!(toc[1].title, "二");
        // 展平顺序 = 显示顺序
        let titles: Vec<&str> = flatten_toc(&toc)
            .iter()
            .map(|entry| entry.title.as_str())
            .collect();
        assert_eq!(titles, vec!["一", "一之一", "一之一之一", "二"]);
    }

    #[test]
    fn rejects_malformed_xml() {
        // 注意：quick-xml 报的是"结束标签不匹配"，不会为"标签未闭合到 EOF"报错，
        // 因此这里用不匹配的结束标签（截断输入会得到部分结果而不是错误，见模块文档）
        let error = parse_nav(
            b"<html><body><nav epub:type=\"toc\"><ol></div></ol></nav></body></html>",
            "/",
        )
        .unwrap_err();
        assert_eq!(error.code(), "EPUB_XML");
    }

    #[test]
    fn rejects_non_utf8() {
        let error = parse_nav(&[0xff, 0xfe], "/").unwrap_err();
        assert_eq!(error.code(), "EPUB_XML");
    }
}
