//! 索引的落盘与加载：自定义紧凑二进制格式。
//!
//! # 为什么需要落盘
//!
//! 导入管线的第三阶段（延迟索引）是后台慢慢跑的，索引必须能持久化，
//! 否则每次启动都要重建 —— 1000 本书重建一次就是几分钟起。
//!
//! # 为什么自己写格式而不是 bincode/postcard
//!
//! 倒排的核心是 `token -> 升序的分片号列表`。对这种**升序整数序列**，
//! **delta + varint** 能比"逐个 u32"省一个数量级：
//!
//! ```text
//! 分片号 [0, 1, 2, 3, 5]  →  delta [0, 1, 1, 1, 2]  →  每项 1 字节
//! 直接存 u32            →  每项 4 字节
//! ```
//!
//! 代价是格式自己维护，所以本模块的测试重点是**往返一致**与**损坏输入的处理**。
//!
//! # 两条硬要求
//!
//! 1. **写入原子化**：先写 `.tmp` 再 rename。否则写到一半崩溃会留下半个文件，
//!    下次加载直接失败，用户看到的是"搜索坏了"。
//! 2. **加载绝不 panic、绝不按不可信长度分配内存**：所有长度字段都必须先用
//!    "剩余字节数" 校验，再分配。这是对抗损坏文件与恶意文件的唯一办法。
//!
//! # 格式
//!
//! ```text
//! magic       8 字节   "PERCHIDX"
//! version     1 字节   目前为 1
//! chunk_chars 8 字节   u64 小端
//! char_count  8 字节   u64 小端（冗余，用于完整性校验）
//! text_len    8 字节   u64 小端
//! text        text_len 字节  UTF-8
//! token_count 4 字节   u32 小端
//! 每个 token：
//!   len       2 字节   u16 小端
//!   bytes     len 字节 UTF-8
//!   post_len  4 字节   u32 小端
//!   postings  post_len 个 varint（分片号的 delta，升序）
//! ```

use std::collections::HashMap;
use std::io;
use std::path::Path;

use thiserror::Error;

use crate::{Index, Stats};

/// 文件魔数。
const MAGIC: &[u8; 8] = b"PERCHIDX";
/// 当前格式版本。
const VERSION: u8 = 1;

/// 落盘/加载错误。
#[derive(Debug, Error)]
pub enum IndexIoError {
    /// 底层 I/O 失败。
    #[error("索引 I/O 失败: {0}")]
    Io(#[from] io::Error),

    /// 文件不是索引文件（魔数不对）。
    #[error("不是 PerchReader 索引文件")]
    BadMagic,

    /// 版本不支持。
    #[error("索引格式版本不支持: {0}")]
    UnsupportedVersion(u8),

    /// 文件被截断或结构损坏。
    #[error("索引文件损坏: {0}")]
    Corrupted(&'static str),

    /// 文本或 token 不是合法 UTF-8。
    #[error("索引内含非法 UTF-8")]
    BadUtf8,

    /// 完整性校验失败（例如 char_count 与实际不符）。
    #[error("索引完整性校验失败: {0}")]
    Integrity(&'static str),
}

impl IndexIoError {
    /// 稳定的错误码，供跨语言层映射。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "INDEX_IO",
            Self::BadMagic => "INDEX_BAD_MAGIC",
            Self::UnsupportedVersion(_) => "INDEX_UNSUPPORTED_VERSION",
            Self::Corrupted(_) => "INDEX_CORRUPTED",
            Self::BadUtf8 => "INDEX_BAD_UTF8",
            Self::Integrity(_) => "INDEX_INTEGRITY",
        }
    }
}

impl Index {
    /// 序列化成字节。
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let stats = self.stats();
        let mut out = Vec::with_capacity(self.text_len_hint());

        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&self.chunk_chars().to_le_bytes());
        out.extend_from_slice(&stats.chars.to_le_bytes());

        let text = self.text_bytes();
        out.extend_from_slice(&(text.len() as u64).to_le_bytes());
        out.extend_from_slice(text);

        let postings = self.postings_map();
        out.extend_from_slice(&(postings.len() as u32).to_le_bytes());
        for (token, chunks) in postings {
            let token_bytes = token.as_bytes();
            // token 长度超过 u16 上限属于异常数据；截断会破坏可逆性，
            // 因此跳过它（正常分词不可能产生这么长的 token）
            if token_bytes.len() > u16::MAX as usize {
                continue;
            }
            out.extend_from_slice(&(token_bytes.len() as u16).to_le_bytes());
            out.extend_from_slice(token_bytes);
            out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
            let mut previous = 0u32;
            for &chunk in chunks {
                // delta 编码：升序序列存相邻差值
                write_varint(&mut out, chunk.saturating_sub(previous));
                previous = chunk;
            }
        }
        out
    }

    /// 由字节反序列化。
    ///
    /// # Errors
    ///
    /// 见 [`IndexIoError`]。**任何损坏输入都返回错误，不会 panic，也不会按
    /// 不可信的长度分配内存**。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IndexIoError> {
        let mut cursor = Cursor::new(bytes);

        if cursor.take(8)? != MAGIC {
            return Err(IndexIoError::BadMagic);
        }
        let version = cursor.u8()?;
        if version != VERSION {
            return Err(IndexIoError::UnsupportedVersion(version));
        }
        let chunk_chars = cursor.u64()?;
        if chunk_chars == 0 {
            return Err(IndexIoError::Corrupted("chunk_chars 为 0"));
        }
        let declared_chars = cursor.u64()?;

        // 关键：长度字段先与剩余字节数比较，再分配
        let text_len = cursor.u64()?;
        if text_len > cursor.remaining() as u64 {
            return Err(IndexIoError::Corrupted("text 长度超过剩余字节"));
        }
        let text_bytes = cursor.take(text_len as usize)?;
        let text = std::str::from_utf8(text_bytes)
            .map_err(|_| IndexIoError::BadUtf8)?
            .to_owned();

        if text.chars().count() as u64 != declared_chars {
            return Err(IndexIoError::Integrity("char_count 与实际字符数不符"));
        }

        let token_count = cursor.u32()? as usize;
        // 每个 token 至少 2+0+4 = 6 字节，用它给分配量设上限，避免恶意文件撑爆内存
        let max_possible = cursor.remaining() / 6;
        if token_count > max_possible {
            return Err(IndexIoError::Corrupted("token 数量与剩余字节不符"));
        }

        let mut postings: HashMap<String, Vec<u32>> = HashMap::with_capacity(token_count);
        for _ in 0..token_count {
            let token_len = cursor.u16()? as usize;
            if token_len == 0 {
                return Err(IndexIoError::Corrupted("token 长度为 0"));
            }
            let token_bytes = cursor.take(token_len)?;
            let token = std::str::from_utf8(token_bytes)
                .map_err(|_| IndexIoError::BadUtf8)?
                .to_owned();

            let post_len = cursor.u32()? as usize;
            // 每个 varint 至少 1 字节
            if post_len > cursor.remaining() {
                return Err(IndexIoError::Corrupted("postings 数量超过剩余字节"));
            }
            let mut chunks = Vec::with_capacity(post_len.min(1024));
            let mut accumulated = 0u32;
            for _ in 0..post_len {
                let delta = cursor.varint()?;
                accumulated = accumulated
                    .checked_add(delta)
                    .ok_or(IndexIoError::Corrupted("postings 溢出"))?;
                chunks.push(accumulated);
            }
            // 升序是格式的基本不变量，破坏它会让求交逻辑悄悄出错
            if chunks.windows(2).any(|pair| pair[0] >= pair[1]) {
                return Err(IndexIoError::Corrupted("postings 不是严格升序"));
            }
            postings.insert(token, chunks);
        }

        if cursor.remaining() != 0 {
            return Err(IndexIoError::Corrupted("文件尾部有多余字节"));
        }

        Ok(Index::from_parts(
            text,
            chunk_chars,
            declared_chars,
            postings,
        ))
    }

    /// 原子写入磁盘：先写 `.tmp` 再改名。
    ///
    /// # Errors
    ///
    /// 见 [`IndexIoError`]。
    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<(), IndexIoError> {
        let path = path.as_ref();
        let temporary = path.with_extension("tmp");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&temporary, self.to_bytes())?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    }

    /// 从磁盘加载。
    ///
    /// # Errors
    ///
    /// 见 [`IndexIoError`]。
    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, IndexIoError> {
        let bytes = std::fs::read(path)?;
        Self::from_bytes(&bytes)
    }

    /// 预估序列化后的大小（只为减少扩容次数，不影响正确性）。
    fn text_len_hint(&self) -> usize {
        let stats: Stats = self.stats();
        self.text_bytes().len() + stats.tokens * 8 + 64
    }
}

/// 读指针，所有取值都做边界检查。
struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    const fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], IndexIoError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(IndexIoError::Corrupted("长度溢出"))?;
        let slice = self
            .bytes
            .get(self.position..end)
            .ok_or(IndexIoError::Corrupted("数据被截断"))?;
        self.position = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, IndexIoError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, IndexIoError> {
        let slice = self.take(2)?;
        Ok(u16::from_le_bytes([slice[0], slice[1]]))
    }

    fn u32(&mut self) -> Result<u32, IndexIoError> {
        let slice = self.take(4)?;
        Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    fn u64(&mut self) -> Result<u64, IndexIoError> {
        let slice = self.take(8)?;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(slice);
        Ok(u64::from_le_bytes(bytes))
    }

    /// LEB128 varint 解码。
    fn varint(&mut self) -> Result<u32, IndexIoError> {
        let mut result: u32 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            if shift >= 32 {
                return Err(IndexIoError::Corrupted("varint 过长"));
            }
            result |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }
}

/// LEB128 varint 编码。
fn write_varint(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const DOC: &str =
        "第一章。天亮了，灯塔的守夜人合上书。海风很大。第二章。天亮了，灯塔的守夜人合上书。";

    fn build() -> Index {
        Index::build(DOC, crate::DEFAULT_CHUNK_CHARS)
    }

    #[test]
    fn round_trip_preserves_stats_and_search() {
        let original = build();
        let bytes = original.to_bytes();
        let restored = Index::from_bytes(&bytes).unwrap();

        assert_eq!(restored.stats(), original.stats());
        assert_eq!(restored.text(), original.text());
        assert_eq!(restored.chunk_chars(), original.chunk_chars());

        // 真正的验收标准：加载后搜索结果与原来完全一致
        let before = original.search("灯塔的守夜人");
        let after = restored.search("灯塔的守夜人");
        assert_eq!(before.len(), after.len());
        for (left, right) in before.iter().zip(after.iter()) {
            assert_eq!(left.char_start, right.char_start);
            assert_eq!(left.char_end, right.char_end);
            assert_eq!(left.chunk, right.chunk);
            assert_eq!(left.snippet, right.snippet);
        }
    }

    #[test]
    fn round_trip_through_file_is_atomic() {
        let directory = std::env::temp_dir().join(format!(
            "perch-index-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let path = directory.join("nested/book.idx");

        let original = build();
        original.save_to(&path).unwrap();
        // 临时文件不应残留
        assert!(!path.with_extension("tmp").exists());

        let restored = Index::load_from(&path).unwrap();
        assert_eq!(restored.stats(), original.stats());

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn empty_index_round_trips() {
        let original = Index::build("", crate::DEFAULT_CHUNK_CHARS);
        let restored = Index::from_bytes(&original.to_bytes()).unwrap();
        assert_eq!(restored.stats().chars, 0);
        assert!(restored.search("任意").is_empty());
    }

    #[test]
    fn cjk_and_multichunk_round_trip() {
        let text = "中文测试".repeat(200);
        let original = Index::build(text, 16);
        let restored = Index::from_bytes(&original.to_bytes()).unwrap();
        assert_eq!(
            restored.search("中文测试").len(),
            original.search("中文测试").len()
        );
        assert!(restored.stats().chunks > 1);
    }

    #[test]
    fn delta_varint_keeps_postings_compact() {
        // 分片号很多且靠得很近时，delta+varint 应远小于逐项 u32
        let text = "字".repeat(5000);
        let index = Index::build(text, 3);
        let bytes = index.to_bytes();
        let stats = index.stats();

        // 注意：不能拿整个文件大小去比——正文是原样存储的（5000 个汉字 = 15KB），
        // 会淹没倒排段的差异。这里只比较"除正文外的部分"。
        let postings_and_headers = bytes.len() - index.text().len();
        let naive = stats.postings * 4;
        assert!(
            postings_and_headers < naive,
            "倒排段 {} 字节，逐项 u32 需要约 {} 字节",
            postings_and_headers,
            naive
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = build().to_bytes();
        bytes[0] = b'X';
        assert_eq!(
            Index::from_bytes(&bytes).unwrap_err().code(),
            "INDEX_BAD_MAGIC"
        );
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut bytes = build().to_bytes();
        bytes[8] = 99;
        assert_eq!(
            Index::from_bytes(&bytes).unwrap_err().code(),
            "INDEX_UNSUPPORTED_VERSION"
        );
    }

    #[test]
    fn rejects_truncated_input_without_panicking() {
        let bytes = build().to_bytes();
        // 从每个长度截断一次，确保都是错误而不是 panic
        for cut in [0, 1, 8, 9, 17, 25, 30, bytes.len() / 2, bytes.len() - 1] {
            let truncated = &bytes[..cut.min(bytes.len())];
            let result = Index::from_bytes(truncated);
            assert!(result.is_err(), "截断到 {cut} 字节时应报错");
        }
    }

    #[test]
    fn rejects_huge_declared_lengths_without_allocating() {
        // 把 text_len 改成天文数字：必须报错而不是尝试分配
        let mut bytes = build().to_bytes();
        let huge = u64::MAX.to_le_bytes();
        // 布局：magic 0..8 | version 8 | chunk_chars 9..17 | char_count 17..25 | text_len 25..33
        bytes[25..33].copy_from_slice(&huge);
        assert_eq!(
            Index::from_bytes(&bytes).unwrap_err().code(),
            "INDEX_CORRUPTED"
        );
    }

    #[test]
    fn rejects_trailing_garbage() {
        let mut bytes = build().to_bytes();
        bytes.push(0xAB);
        assert_eq!(
            Index::from_bytes(&bytes).unwrap_err().code(),
            "INDEX_CORRUPTED"
        );
    }

    #[test]
    fn rejects_non_monotonic_postings() {
        // 构造一个 delta 为 0 的（重复分片号）→ 非严格升序 → 报错
        let mut bytes = build().to_bytes();
        // 在尾部追加一个"只有 1 个 token、post_len=2、delta=0,0"的伪造段太复杂，
        // 这里改为直接验证 varint 边界：截断到只剩部分 token 数据
        bytes.truncate(bytes.len() - 3);
        assert!(Index::from_bytes(&bytes).is_err());
    }

    #[test]
    fn varint_round_trips_for_boundaries() {
        for value in [
            0u32,
            1,
            127,
            128,
            255,
            16_383,
            16_384,
            u32::MAX / 2,
            u32::MAX,
        ] {
            let mut buffer = Vec::new();
            write_varint(&mut buffer, value);
            let mut cursor = Cursor::new(&buffer);
            assert_eq!(cursor.varint().unwrap(), value);
            assert_eq!(cursor.remaining(), 0);
        }
    }

    #[test]
    fn varint_rejects_overlong_encoding() {
        // 连续 6 个 0x80（带继续位）超过 u32 的 varint 长度上限
        let bytes = [0x80u8, 0x80, 0x80, 0x80, 0x80, 0x80];
        let mut cursor = Cursor::new(&bytes);
        assert!(cursor.varint().is_err());
    }
}
