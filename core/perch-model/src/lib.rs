//! PerchReader 核心数据模型。
//!
//! 本 crate 是**纯数据 + 纯校验**，不含 I/O、不含平台依赖、不含 FFI。
//! 设计约束（见 `docs/02-架构设计.md`）：
//!
//! 1. **零 C 依赖**——保证在 Linux 上可为 `aarch64-apple-ios` 交叉编译与校验；
//! 2. **不 panic**——所有可预期失败都返回 `Result`，这是"崩溃率 < 0.1%"的第一道防线；
//! 3. **跨语言中立**——不出现 iOS/Foundation/Flutter 概念，未来 Kotlin/WASM 可原样复用。
//!
//! 核心类型：
//! - [`Locator`]：统一位置表示，JSON 结构与 Readium 的 Locator 互操作
//! - [`Book`]：书籍元数据与导入状态机
//! - [`Annotation`]：高亮 / 笔记 / 书签，带同步所需的墓碑与 HLC

#![forbid(unsafe_code)]

pub mod annotation;
pub mod book;
pub mod error;
pub mod locator;
pub mod timestamp;

mod util;

pub use annotation::{Annotation, AnnotationId, AnnotationKind};
pub use book::{Book, BookId, ImportState};
pub use error::ModelError;
pub use locator::{Format, LOCATOR_VERSION, Locations, Locator, Perch, TextContext};
pub use timestamp::Timestamp;
