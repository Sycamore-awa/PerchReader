//! 书脊定位：把 CFI 与 `(href, 字节偏移)` 互相换算。
//!
//! # 它补上了哪一环
//!
//! [`crate::cfi`] 只认识 CFI 文本，[`crate::xhtml`] 只认识**单个**内容文档内部的
//! "字节偏移 ↔ 文本节点路径"。本模块把两者接起来：
//!
//! - [`resolve_cfi`]：点 CFI → `(href, 字节偏移)`
//! - [`cfi_of`]：`(href, 字节偏移)` → 点 CFI
//! - [`resolve_range_cfi`]：范围 CFI → 两个端点位置（允许跨资源）
//!
//! 所有函数都**不碰文件系统**：内容由调用方给的 `read` 闭包按需读取。
//! 这样既能配合 [`crate::EpubArchive`] 使用，也能在测试里直接喂内存字节。
//!
//! # 书脊序号与"跳过非 XHTML 项"的规则（重要）
//!
//! CFI 里指向内容文档的那一段形如 `/6/4[chap01]!`：`/6` 是 OPF 的 `spine` 元素，
//! `/4` 是 `spine` 里的第 2 个 `<itemref>`（CFI 索引从 1 开始、偶数表示元素，
//! 所以第 n 项是 `/2n`）。本模块把**1 基的 `<itemref>` 序号**称作"书脊序号"。
//!
//! 公开函数不直接接收 [`Package`]，而是接收一个 `spine: &[&str]` 切片——它是
//! "可定位的内容文档 href 序列"。这个切片必须由 [`spine_content_hrefs`] 生成
//! （或遵循同样规则），规则是：
//!
//! 1. 按 `spine` 中 `<itemref>` 的原始顺序；
//! 2. `idref` 指向不存在的清单项时跳过（沿用 [`Package::spine_items`] 的语义）；
//! 3. **跳过非 XHTML 项**（[`crate::package::ManifestItem::is_xhtml`] 为 `false`）。典型是误把
//!    图片/NCX 塞进 spine 的破损书。
//!
//! 于是 `spine[n - 1]` 就是 CFI 里书脊序号 `n` 对应的资源；生成与解析两侧都用
//! 这一个切片，所以天然互逆。对合法 EPUB（spine 只含 XHTML）过滤不会改变顺序；
//! 对破损书，本模块宁愿让序号只覆盖"真正能定位的章节"，也不把不可读的资源算进
//! 序号——这是**有意选择**，与外部阅读器按原始 `<itemref>` 计数可能不同。
//!
//! # `Ok(None)` 与 `Err` 的分界
//!
//! - **定位不到 → `Ok(None)`**：CFI 给出的书脊序号越界；`href` 不在 `spine` 里；
//!   字节偏移不落在任何文本节点上；范围 CFI 实际是点定位；CFI 结构上给不出书脊
//!   序号（例如没有 `!` 左侧的间接路径，或间接路径末段不是元素步）。
//! - **输入本身坏了 → `Err`**：CFI 文本语法错（[`EpubError::Cfi`]）；目标内容读不到
//!   （`read` 闭包返回的错误原样传出）；目标 XHTML 无法解析（[`EpubError::Xhtml`]）。
//!
//! 换句话说："能解析、但换算不出位置"给 `None`；"连解析/读取都失败"给 `Err`。
//!
//! # `read` 闭包
//!
//! `read` 收到的是 [`EpubLocation::href`] 同形的**容器绝对路径**（例如
//! `/OEBPS/text/chap01.xhtml`），不是 ZIP 条目名。配 [`crate::EpubArchive`] 使用时
//! 由调用方去掉前导 `/`（见 [`crate::package::ManifestItem::entry_name`]）。
//!
//! [`resolve_range_cfi`] 可能调用 `read` 最多两次（同资源会各解析一次，跨资源会用
//! 两个不同 href），因此闭包必须是 `FnMut`，且每次调用都应返回同一份内容。

use crate::cfi::{Cfi, CfiPath, Step};
use crate::error::EpubError;
use crate::package::Package;
use crate::xhtml::XhtmlIndex;

/// CFI 里 `spine` 元素在 OPF `package` 元素下的标准步索引。
///
/// OPF 的元素子节点顺序是 `metadata`(/2)、`manifest`(/4)、`spine`(/6)，CFI 规范
/// 里的标准包内路径就是 `/6/{itemref}`。
const SPINE_STEP: u32 = 6;

/// 书脊内的位置：哪个资源 + 该资源内的字节偏移。
///
/// `href` 是**容器绝对路径**（带前导 `/`），与 [`crate::package::ManifestItem::href`] 同形；
/// `byte_offset` 是该 XHTML **原始字节**里的偏移，不是字符偏移。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EpubLocation {
    /// 内容文档的容器绝对路径（如 `/OEBPS/text/chap01.xhtml`）。
    pub href: String,
    /// 该资源内的原始字节偏移。
    pub byte_offset: usize,
}

impl EpubLocation {
    /// 构造一个书脊位置。
    #[must_use]
    pub fn new(href: impl Into<String>, byte_offset: usize) -> Self {
        Self {
            href: href.into(),
            byte_offset,
        }
    }
}

/// 由 OPF 包文档得到"可定位书脊项"的 href 序列（书脊序号的来源）。
///
/// 规则（也是本模块所有函数对 `spine` 参数的约定）：
///
/// 1. 按 `<itemref>` 的原始顺序；
/// 2. 跳过 `idref` 悬空的项——这一步由 [`Package::spine_items`] 完成；
/// 3. 跳过非 XHTML 项——CFI 只能定位到可解析的文本内容文档。
///
/// 返回的 `Vec` 里第 `n - 1` 个元素就是 CFI 书脊序号 `n` 指向的资源。
#[must_use]
pub fn spine_content_hrefs(package: &Package) -> Vec<&str> {
    package
        .spine_items()
        .into_iter()
        .filter(|item| item.is_xhtml())
        .map(|item| item.href.as_str())
        .collect()
}

/// 把**点 CFI**解析成 `(href, 字节偏移)`。
///
/// `spine` 必须是由 [`spine_content_hrefs`] 生成（或遵循同样规则）的可定位
/// 内容文档序列；`read` 按 href 读取该资源的原始字节。
///
/// # 返回
///
/// - `Ok(Some(location))`：解析成功。
/// - `Ok(None)`：书脊序号越界、资源里找不到对应文本节点、或 CFI 结构上给不出
///   书脊序号（缺少 `!` 左侧间接路径 / 末段不是元素步）。
///
/// # Errors
///
/// - CFI 文本非法（含把范围 CFI 传给本函数）→ [`EpubError::Cfi`]。
/// - `read` 返回错误（资源读不到）→ 原样传出。
/// - XHTML 解析失败 → [`EpubError::Xhtml`]。
pub fn resolve_cfi(
    cfi: &str,
    spine: &[&str],
    mut read: impl FnMut(&str) -> Result<Vec<u8>, EpubError>,
) -> Result<Option<EpubLocation>, EpubError> {
    // 复用 CfiPath::parse：它内部走 Cfi::parse，并要求恰好是一条路径。
    let path = CfiPath::parse(cfi)?;
    resolve_path(&path, spine, &mut read)
}

/// 把 `(href, 字节偏移)` 生成**点 CFI**。
///
/// `spine` 的约定同 [`resolve_cfi`]。`read` 只对 `href` 调用一次。
///
/// # 返回
///
/// - `Ok(Some(cfi))`：偏移落在某个文本节点内，已生成 CFI。
/// - `Ok(None)`：`href` 不在 `spine` 里；或偏移不落在任何文本节点上
///   （例如落在标签/注释里，或超出文档末尾）。
///
/// # Errors
///
/// - `read` 返回错误（资源读不到）→ 原样传出。
/// - XHTML 解析失败 → [`EpubError::Xhtml`]。
pub fn cfi_of(
    href: &str,
    byte_offset: usize,
    spine: &[&str],
    mut read: impl FnMut(&str) -> Result<Vec<u8>, EpubError>,
) -> Result<Option<Cfi>, EpubError> {
    let Some(position) = spine.iter().position(|candidate| *candidate == href) else {
        return Ok(None);
    };
    let Ok(ordinal) = u32::try_from(position + 1) else {
        return Ok(None);
    };

    let bytes = read(href)?;
    let index = XhtmlIndex::build(&bytes)?;
    let Some(located) = index.locate_byte(byte_offset) else {
        return Ok(None);
    };

    let indirection = vec![Step::new(SPINE_STEP, None), Step::element_child(ordinal)];
    let path = CfiPath::new(indirection, located.path.local, located.path.text_offset);
    Ok(Some(Cfi::point(path)))
}

/// 把**范围 CFI**解析成两个端点位置。
///
/// 两端各自按自己的书脊序号解析，因此**可以落在同一个资源，也可以跨章**：
///
/// ```text
/// 同资源：epubcfi(/6/2!/2/2/2,/1:0,/1:5)
/// 跨资源：epubcfi(/6,/2!/2/2/2/1:0,/4!/2/2/2/1:5)
/// ```
///
/// 跨资源的写法依赖 CFI 的"端点自带 `!`"，由 [`Cfi::expand_range`] 按规范拼接。
///
/// # 返回
///
/// - `Ok(Some((start, end)))`：两个端点都解析成功。**不排序**：原样保留 CFI 里
///   给出的起止顺序；调用方若需要"前 < 后"应自行比较。
/// - `Ok(None)`：输入是点定位（没有范围可解析），或任一端点定位不到。
///
/// # Errors
///
/// - CFI 文本非法、段数不是 1/3、或父路径与端点同时带 `!`
///   → [`EpubError::Cfi`]。
/// - `read` 返回错误、XHTML 解析失败 → 原样传出 / [`EpubError::Xhtml`]。
pub fn resolve_range_cfi(
    cfi: &str,
    spine: &[&str],
    mut read: impl FnMut(&str) -> Result<Vec<u8>, EpubError>,
) -> Result<Option<(EpubLocation, EpubLocation)>, EpubError> {
    let cfi = Cfi::parse(cfi)?;
    let Some((start, end)) = cfi.expand_range()? else {
        return Ok(None);
    };
    let Some(start_path) = start.primary() else {
        return Ok(None);
    };
    let Some(end_path) = end.primary() else {
        return Ok(None);
    };

    let Some(start_location) = resolve_path(start_path, spine, &mut read)? else {
        return Ok(None);
    };
    let Some(end_location) = resolve_path(end_path, spine, &mut read)? else {
        return Ok(None);
    };
    Ok(Some((start_location, end_location)))
}

/// 解析一条已经拆好的点路径（供 [`resolve_cfi`] / [`resolve_range_cfi`] 共用）。
fn resolve_path(
    path: &CfiPath,
    spine: &[&str],
    read: &mut impl FnMut(&str) -> Result<Vec<u8>, EpubError>,
) -> Result<Option<EpubLocation>, EpubError> {
    let Some(ordinal) = spine_ordinal(path) else {
        return Ok(None);
    };
    let Ok(index) = usize::try_from(ordinal) else {
        return Ok(None);
    };
    let Some(href) = spine.get(index.saturating_sub(1)) else {
        return Ok(None);
    };

    let bytes = read(href)?;
    let index = XhtmlIndex::build(&bytes)?;
    let Some(byte_offset) = byte_offset_in(&index, path) else {
        return Ok(None);
    };
    Ok(Some(EpubLocation::new(*href, byte_offset)))
}

/// 从 CFI 路径里取 1 基书脊序号。
///
/// 约定：`indirection` 的最后一段是 `<itemref>`（偶数索引），序号 = 索引 / 2。
/// 没有间接路径、或末段不是元素步时返回 `None`（"定位不到"，不是"输入坏了"）。
fn spine_ordinal(path: &CfiPath) -> Option<u32> {
    let itemref = path.indirection.last()?;
    if !itemref.is_element() {
        return None;
    }
    Some(itemref.index / 2)
}

/// 把 CFI 路径换算成**原始字节偏移**。
///
/// [`XhtmlIndex::byte_offset_of`] 只给文本节点起点；这里再把 `text_offset`
/// （端点内的字符偏移）叠加上去。实体引用会让"解码后的字符"和"原始字节"的
/// 边界错开，所以按解码后的字符逐个映射回原始字节，而不是用"字符偏移 ×
/// UTF-8 宽度"。
///
/// 偏移落在实体引用内部（`&am` 中间）时吸附到该实体的起点——这与
/// [`XhtmlIndex::locate_byte`] "宁可略偏也不失败"的取向一致。
fn byte_offset_in(index: &XhtmlIndex, path: &CfiPath) -> Option<usize> {
    let node = index.resolve(path)?;
    let target = usize::try_from(path.text_offset.unwrap_or(0)).ok()?;
    let total = node.text.chars().count();
    if target > total {
        return None;
    }
    if target == 0 {
        return Some(node.range.start);
    }

    let raw = index.source().get(node.range.clone())?;
    let mut decoded_chars = 0usize;
    let mut cursor = 0usize;
    while cursor < raw.len() {
        if decoded_chars == target {
            return Some(node.range.start + cursor);
        }
        if raw.as_bytes().get(cursor) == Some(&b'&')
            && let Some(semi) = raw[cursor..].find(';')
        {
            let end = cursor + semi + 1;
            if let Ok(decoded) = quick_xml::escape::unescape(&raw[cursor..end]) {
                let count = decoded.chars().count().max(1);
                if decoded_chars + count > target {
                    // 偏移落在实体解码出的字符内部：吸附到实体起点。
                    return Some(node.range.start + cursor);
                }
                decoded_chars += count;
                cursor = end;
                continue;
            }
        }
        let ch = raw.get(cursor..)?.chars().next()?;
        decoded_chars += 1;
        cursor += ch.len_utf8();
    }

    if decoded_chars >= target {
        Some(node.range.end)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::cfi::CfiError;

    const CH1: &str = "<html><body><p>第一章甲</p><p>第一章乙</p></body></html>";
    const CH2: &str = "<html><body><p>第二章甲</p></body></html>";
    const CH3: &str = "<html><body><p>第三章甲</p></body></html>";

    const SPINE: [&str; 3] = [
        "/OEBPS/text/chap01.xhtml",
        "/OEBPS/text/chap02.xhtml",
        "/OEBPS/text/chap03.xhtml",
    ];

    /// 每次返回一个"内存 EPUB 读取器"。
    fn reader() -> impl FnMut(&str) -> Result<Vec<u8>, EpubError> {
        |href| match href {
            "/OEBPS/text/chap01.xhtml" => Ok(CH1.as_bytes().to_vec()),
            "/OEBPS/text/chap02.xhtml" => Ok(CH2.as_bytes().to_vec()),
            "/OEBPS/text/chap03.xhtml" => Ok(CH3.as_bytes().to_vec()),
            other => Err(EpubError::EntryNotFound(other.to_owned())),
        }
    }

    /// 第一个文本节点的字节起点。
    fn first_text_offset(xhtml: &str) -> usize {
        XhtmlIndex::build(xhtml.as_bytes())
            .unwrap()
            .text_nodes()
            .first()
            .unwrap()
            .range
            .start
    }

    #[test]
    fn round_trips_every_text_node_offset() {
        let index = XhtmlIndex::build(CH1.as_bytes()).unwrap();
        // 覆盖首/中/尾：节点起点（CFI 字符偏移 0）、节点内一个字符之后、节点末尾。
        let offsets: Vec<usize> = index
            .text_nodes()
            .iter()
            .flat_map(|node| {
                let middle = if node.range.start + 3 < node.range.end {
                    node.range.start + 3
                } else {
                    node.range.start
                };
                [node.range.start, middle, node.range.end]
            })
            .collect();

        for offset in offsets {
            let cfi = cfi_of(SPINE[0], offset, &SPINE, reader())
                .unwrap()
                .unwrap_or_else(|| panic!("offset {offset} 应能生成 CFI"));
            let resolved = resolve_cfi(&cfi.to_string(), &SPINE, reader())
                .unwrap()
                .unwrap_or_else(|| panic!("{cfi} 应能解析回位置"));
            assert_eq!(resolved.href, SPINE[0]);
            assert_eq!(resolved.byte_offset, offset, "CFI = {cfi}");
        }
    }

    #[test]
    fn generated_cfi_uses_standard_spine_path() {
        let offset = first_text_offset(CH1);
        let cfi = cfi_of(SPINE[0], offset, &SPINE, reader()).unwrap().unwrap();
        // html(/2) → body(/2) → p(/2) → 文本(/1)
        assert_eq!(cfi.to_string(), "/6/2!/2/2/2/1:0");
        assert!(!cfi.is_range());
    }

    #[test]
    fn resolves_second_and_third_spine_items() {
        for (href, itemref_index) in [
            ("/OEBPS/text/chap02.xhtml", 4u32),
            ("/OEBPS/text/chap03.xhtml", 6u32),
        ] {
            let offset = first_text_offset(match href {
                "/OEBPS/text/chap02.xhtml" => CH2,
                _ => CH3,
            });
            let cfi = cfi_of(href, offset, &SPINE, reader()).unwrap().unwrap();
            let primary = cfi.primary().unwrap();
            assert_eq!(primary.indirection[1].index, itemref_index);
            assert_eq!(
                primary.indirection[1],
                Step::element_child(itemref_index / 2)
            );

            let resolved = resolve_cfi(&cfi.to_string(), &SPINE, reader())
                .unwrap()
                .unwrap();
            assert_eq!(resolved.href, href);
            assert_eq!(resolved.byte_offset, offset);
        }
    }

    #[test]
    fn resolves_same_resource_range() {
        // 父路径 /6/2!/2/2/2，起点/终点都追加 /1 并给不同 text_offset。
        let (start, end) = resolve_range_cfi("epubcfi(/6/2!/2/2/2,/1:0,/1:1)", &SPINE, reader())
            .unwrap()
            .unwrap();

        assert_eq!(start.href, SPINE[0]);
        assert_eq!(end.href, SPINE[0]);
        assert!(start.byte_offset < end.byte_offset, "{start:?} vs {end:?}");
        // 第二个端点比第一个多走一个汉字（3 字节）
        assert_eq!(end.byte_offset, start.byte_offset + 3);
    }

    #[test]
    fn resolves_cross_resource_range() {
        // 跨章：父路径只到 spine，两个端点各自带 `!`。
        let (start, end) =
            resolve_range_cfi("epubcfi(/6,/2!/2/2/2/1:0,/4!/2/2/2/1:1)", &SPINE, reader())
                .unwrap()
                .unwrap();

        assert_eq!(start.href, SPINE[0]);
        assert_eq!(end.href, SPINE[1]);
        assert_ne!(start.href, end.href);
        assert_eq!(end.byte_offset, first_text_offset(CH2) + 3);
    }

    #[test]
    fn point_cfi_has_no_range() {
        assert_eq!(
            resolve_range_cfi("epubcfi(/6/2!/2/2/2/1:0)", &SPINE, reader()).unwrap(),
            None
        );
    }

    #[test]
    fn malformed_cfi_is_an_error() {
        let error = resolve_cfi("epubcfi(/6/2", &SPINE, reader()).unwrap_err();
        assert_eq!(error.code(), "EPUB_CFI");
        assert!(matches!(error, EpubError::Cfi(CfiError::UnclosedWrapper)));

        // 两段式不是合法 range
        let error = resolve_range_cfi("epubcfi(/6/2!/2/2/2/1:0,/6/4!/2/2/2/1:1)", &SPINE, reader())
            .unwrap_err();
        assert_eq!(error.code(), "EPUB_CFI");
        assert!(matches!(error, EpubError::Cfi(CfiError::RangeArity(2))));
    }

    #[test]
    fn out_of_range_spine_index_is_none() {
        // /8 是第 4 个 itemref，书脊只有 3 项
        assert_eq!(
            resolve_cfi("epubcfi(/6/8!/2/2/2/1:0)", &SPINE, reader()).unwrap(),
            None
        );
    }

    #[test]
    fn unknown_href_is_none_without_reading() {
        // `read` 一旦被调用就 panic：href 不在 spine 里时不应读资源
        let result = cfi_of(
            "/OEBPS/text/unknown.xhtml",
            0,
            &SPINE,
            |_| -> Result<Vec<u8>, EpubError> { panic!("不应读取资源") },
        )
        .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn offset_outside_text_is_none() {
        // 0 落在 `<html>` 标签里，不是文本节点
        assert_eq!(cfi_of(SPINE[0], 0, &SPINE, reader()).unwrap(), None);
        // 超出文档末尾
        assert_eq!(
            cfi_of(SPINE[0], CH1.len() + 10, &SPINE, reader()).unwrap(),
            None
        );
    }

    #[test]
    fn read_and_xhtml_errors_are_propagated() {
        let error = cfi_of(SPINE[0], 0, &SPINE, |_| {
            Err(EpubError::EntryNotFound("missing".to_owned()))
        })
        .unwrap_err();
        assert_eq!(error.code(), "EPUB_ENTRY_NOT_FOUND");

        let error =
            cfi_of(SPINE[0], 0, &SPINE, |_| Ok(b"<html><body></html>".to_vec())).unwrap_err();
        assert_eq!(error.code(), "EPUB_XHTML");
    }

    #[test]
    fn spine_content_hrefs_skips_non_xhtml_and_dangling_idrefs() {
        let opf = r#"<package><manifest>
            <item id="img" href="images/cover.jpg" media-type="image/jpeg"/>
            <item id="c1" href="text/c1.xhtml" media-type="application/xhtml+xml"/>
            <item id="c2" href="text/c2.xhtml" media-type="application/xhtml+xml"/>
        </manifest><spine>
            <itemref idref="img"/>
            <itemref idref="ghost"/>
            <itemref idref="c1"/>
            <itemref idref="c2"/>
        </spine></package>"#;
        let package = Package::parse("content.opf", opf).unwrap();

        // spine_items() 只跳过悬空 idref，图片项仍在
        assert_eq!(package.spine_items().len(), 3);

        // 本模块的序号只覆盖 XHTML：img 与 ghost 都被跳过
        let spine = spine_content_hrefs(&package);
        assert_eq!(spine, vec!["/text/c1.xhtml", "/text/c2.xhtml"]);

        let mut read = |href: &str| match href {
            "/text/c1.xhtml" => Ok(CH1.as_bytes().to_vec()),
            "/text/c2.xhtml" => Ok(CH2.as_bytes().to_vec()),
            other => Err(EpubError::EntryNotFound(other.to_owned())),
        };
        let offset = first_text_offset(CH2);
        let cfi = cfi_of("/text/c2.xhtml", offset, &spine, &mut read)
            .unwrap()
            .unwrap();
        // c2 是过滤后第 2 项 → /4
        assert_eq!(cfi.primary().unwrap().indirection[1].index, 4);

        let resolved = resolve_cfi(&cfi.to_string(), &spine, &mut read)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.href, "/text/c2.xhtml");
        assert_eq!(resolved.byte_offset, offset);
    }

    #[test]
    fn works_with_epub_archive_and_package() {
        use crate::EpubArchive;
        use crate::test_fixture;

        let mut archive = EpubArchive::from_bytes(test_fixture::minimal_epub()).unwrap();
        let opf_path = archive.opf_path().unwrap();
        let opf = archive.read_to_string(&opf_path).unwrap();
        let package = Package::parse(&opf_path, &opf).unwrap();

        let spine = spine_content_hrefs(&package);
        assert_eq!(spine.len(), 3, "夹具书脊是 3 个 XHTML 章节");

        let href = spine[1];
        let entry = href.strip_prefix('/').unwrap_or(href);
        let bytes = archive.read(entry).unwrap();
        let offset = XhtmlIndex::build(&bytes).unwrap().text_nodes()[0]
            .range
            .start;

        // 调用方负责把"容器绝对路径"换成 ZIP 条目名。
        let mut read = |wanted: &str| {
            let name = wanted.strip_prefix('/').unwrap_or(wanted);
            archive.read(name)
        };
        let cfi = cfi_of(href, offset, &spine, &mut read).unwrap().unwrap();
        let located = resolve_cfi(&cfi.to_string(), &spine, &mut read)
            .unwrap()
            .unwrap();
        assert_eq!(located.href, href);
        assert_eq!(located.byte_offset, offset);
    }

    #[test]
    fn missing_indirection_is_none() {
        // 没有 `!` 左侧的书脊路径，无法映射到某个资源
        assert_eq!(resolve_cfi("/2/2/2/1:0", &SPINE, reader()).unwrap(), None);
    }

    #[test]
    fn range_with_unresolvable_endpoint_is_none() {
        // 起点能定位，终点指向不存在的文本节点 → 整体 None
        assert_eq!(
            resolve_range_cfi("epubcfi(/6,/2!/2/2/2/1:0,/4!/2/2/2/9:0)", &SPINE, reader()).unwrap(),
            None
        );
    }

    #[test]
    fn entity_text_offsets_round_trip() {
        let xhtml = "<html><body><p>甲&amp;乙</p></body></html>";
        let index = XhtmlIndex::build(xhtml.as_bytes()).unwrap();
        let node = &index.text_nodes()[0];
        assert_eq!(node.text, "甲&乙");

        let spine = ["/a.xhtml"];
        let mut read = |_: &str| Ok(xhtml.as_bytes().to_vec());
        for offset in [
            node.range.start,
            node.range.start + 3,
            node.range.start + 8,
            node.range.end,
        ] {
            let cfi = cfi_of("/a.xhtml", offset, &spine, &mut read)
                .unwrap()
                .unwrap();
            let resolved = resolve_cfi(&cfi.to_string(), &spine, &mut read)
                .unwrap()
                .unwrap();
            assert_eq!(resolved.byte_offset, offset, "CFI = {cfi}");
        }
    }
}
