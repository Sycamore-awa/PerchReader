//! EPUB 容器（ZIP）与 OPF 包文档解析。
//!
//! # 分层
//!
//! - [`EpubArchive`]：ZIP 容器层。负责读条目、解析 `META-INF/container.xml` 找到 OPF。
//! - [`Package`]：OPF（package document）层。解析元数据、清单（manifest）、书脊（spine）。
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
//! - CFI 范围定位的"相对路径 → 绝对路径"展开（点定位已可用，见 [`cfi`]）
//! - 目录（TOC）：EPUB3 的 NAV 文档与 EPUB2 的 NCX
//! - 全文抽取（供索引使用）

#![forbid(unsafe_code)]

pub mod archive;
pub mod cfi;
pub mod error;
pub mod package;
mod xml;

pub use archive::EpubArchive;
pub use cfi::{Cfi, CfiError, CfiPath, Step};
pub use error::EpubError;
pub use package::{ManifestItem, Metadata, Package, SpineItem, resolve_href};

#[cfg(test)]
pub(crate) mod test_fixture;
