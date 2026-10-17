//! EPUB 容器（ZIP）与 OPF 包文档解析。
//!
//! # 分层
//!
//! - [`EpubArchive`]：ZIP 容器层。负责读条目、解析 `META-INF/container.xml` 找到 OPF。
//! - [`Package`]：OPF（package document）层。解析元数据、清单（manifest）、书脊（spine）。
//! - [`toc`]：目录层。把 EPUB3 的 NAV 与 EPUB2 的 NCX 统一成同一种树。
//! - [`cfi`] / [`xhtml`]：定位层。CFI 的读写，以及"字节偏移 ↔ CFI"的换算。
//! - [`epub_location`]：跨资源定位层。把书脊里的 CFI 与 `(href, 字节偏移)` 互转。
//!
//! 刻意**不**在这一层做渲染、不做 DOM、不碰 Locator：这里只把"书的结构"变成 Rust 数据结构。
//!
//! # 路径约定（重要）
//!
//! 对外一律使用**容器绝对路径**（带前导 `/`），例如 `/OEBPS/chap01.xhtml`。
//! 这与 Readium 的 Locator `href` 形式一致，也与 `perch-model::Locator` 的约定一致。
//! 而 ZIP 条目名不带前导 `/`，两者之间的转换由
//! [`ManifestItem::entry_name`] / [`crate::archive::EpubArchive::read`] 负责。
//!
//! # 尚未实现（下一步）
//!
//! - CFI 包内路径目前固定写成规范形式 `/6/{itemref}`；非标准 OPF 元素顺序
//!   （把 `spine` 放到别的位置）尚未自适应
//! - ~~全文抽取（供索引使用）~~ → **已由 `perch-jobs` 实现**：
//!   [`perch_jobs::extract_epub_text`] 把书脊读成纯文本 + 章节区间映射，
//!   [`perch_jobs::index_book`] 直接喂给 `perch-index`。
//!   本 crate 仍然只做"结构 ↔ 定位"，不承担"拼出整本书"的职责。

#![forbid(unsafe_code)]

pub mod anchor;
pub mod archive;
pub mod cfi;
pub mod epub_location;
pub mod error;
pub mod package;
pub mod toc;
pub mod xhtml;
mod xml;

pub use anchor::{AnchorQuery, Anchored, MIN_CONFIDENCE, reanchor};
pub use archive::EpubArchive;
pub use cfi::{Cfi, CfiError, CfiPath, Step};
pub use epub_location::{
    EpubLocation, cfi_of, resolve_cfi, resolve_range_cfi, spine_content_hrefs,
};
pub use error::EpubError;
pub use package::{ManifestItem, Metadata, Package, SpineItem, resolve_href};
pub use toc::{TocEntry, flatten_toc, parse_nav, parse_ncx};
pub use xhtml::{Located, TextNode, XhtmlError, XhtmlIndex};

#[cfg(test)]
pub(crate) mod test_fixture;
