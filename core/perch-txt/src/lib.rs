//! 纯文本（TXT）处理：编码探测、解码、行列索引、字符/字节偏移换算。
//!
//! # 为什么 TXT 需要专门一个 crate
//!
//! TXT 看起来是最简单的格式，但对**中文阅读器**来说是坑最多的一个：
//!
//! 1. **编码**：大量中文 TXT 是 GBK / GB18030 / Big5，不是 UTF-8。
//!    解错编码得到的是乱码，而"猜错"比"报错"更糟——用户不知道发生了什么。
//! 2. **偏移的双重性**：TXT 的 Locator 用**字符**偏移（跨端稳定、与渲染无关），
//!    但切片文本要用**字节**偏移。中文一个字 3 字节，两者必须能精确互换。
//! 3. **大文件**：50MB 的 TXT 合集不能每次定位都从头数一遍字符。
//!
//! # 两个关键设计
//!
//! ## 1. 抽样字符索引（[`CharIndex`]）
//!
//! 如果为每个字符存一个字节偏移，300 万字的书要 24MB 内存。所以**每 256 个字符才记一次**
//! 字节偏移：内存降到约 1/256（300 万字 → 96KB），而查询代价只是"从最近的抽样点往前走
//! 最多 255 个字符"——可以忽略。`byte → char` 同理，先用二分查找定位抽样点。
//!
//! ## 2. 编码探测的"确定性"分级
//!
//! - **BOM** 是确定的
//! - **合法的 UTF-8** 基本可以确定（GBK 中文很难恰好构成合法 UTF-8）
//! - 其余只能交给 [`chardetng`] **猜**，而猜是会错的
//!
//! 因此 [`TxtMeta::certain_encoding`] 会如实告诉调用方"这是猜的"，
//! 配合 [`TxtDocument::with_encoding`] 让 UI 提供"换个编码重读"。
//! 对中文用户来说，这个功能不是锦上添花，而是必需品。

use std::ops::Range;

use encoding_rs::Encoding;
use perch_model::Locator;

/// 抽样步长：每多少个字符记录一次字节偏移。
///
/// 越小越准也越费内存。256 是个折中：查询最多走 255 个字符，内存约为 1/256。
const SAMPLE_STRIDE: u64 = 256;

/// 探测到的编码信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxtMeta {
    /// 实际使用的编码名（`encoding_rs` 的名字，如 `UTF-8` / `GBK` / `Big5`）。
    pub encoding: &'static str,
    /// 编码是否是**确定**的（来自 BOM、合法 UTF-8、或用户显式指定）。
    ///
    /// 为 `false` 时说明这是 `chardetng` 的猜测，UI 应当提供"换个编码重读"的入口。
    pub certain_encoding: bool,
    /// 原始字节数。
    pub source_bytes: usize,
    /// 解码后的字符数。
    pub chars: u64,
    /// 行数。
    pub lines: usize,
    /// 解码时是否出现过非法序列（出现即说明编码可能选错了）。
    pub had_decode_errors: bool,
}

/// 一个纯文本文档。
pub struct TxtDocument {
    text: String,
    index: CharIndex,
    /// 每行起始的**字符**偏移（`starts[0]` 恒为 0）。
    line_starts: Vec<u64>,
    meta: TxtMeta,
}

impl TxtDocument {
    /// 读取磁盘上的 TXT 文件。
    ///
    /// # Errors
    ///
    /// 读取失败时返回 [`std::io::Error`]。
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, std::io::Error> {
        let bytes = std::fs::read(path)?;
        Ok(Self::from_bytes(&bytes))
    }

    /// 由字节构造：先探测编码，再解码，再建索引。
    ///
    /// 解码**永远不会失败**（非法序列会被替换为 U+FFFD），因为"解码出乱码"也好过"打不开书"；
    /// 是否出现过替换可以从 [`TxtMeta::had_decode_errors`] 看出来。
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let detected = detect_and_decode(bytes);
        Self::assemble(
            detected.text.into_owned(),
            detected.encoding,
            detected.certain,
            bytes.len(),
            detected.had_errors,
        )
    }

    /// 用指定编码解码（供 UI 的"换个编码重读"使用）。
    ///
    /// 返回 `None` 表示编码名无法识别。
    #[must_use]
    pub fn with_encoding(bytes: &[u8], label: &str) -> Option<Self> {
        let encoding = Encoding::for_label(label.as_bytes())?;
        let (text, _, had_errors) = encoding.decode(bytes);
        Some(Self::assemble(
            text.into_owned(),
            encoding.name(),
            true,
            bytes.len(),
            had_errors,
        ))
    }

    fn assemble(
        text: String,
        encoding: &'static str,
        certain_encoding: bool,
        source_bytes: usize,
        had_decode_errors: bool,
    ) -> Self {
        let index = CharIndex::build(&text);
        let line_starts = build_line_starts(&text);
        let meta = TxtMeta {
            encoding,
            certain_encoding,
            source_bytes,
            chars: index.chars,
            lines: line_starts.len(),
            had_decode_errors,
        };
        Self {
            text,
            index,
            line_starts,
            meta,
        }
    }

    /// 元信息。
    #[must_use]
    pub fn meta(&self) -> &TxtMeta {
        &self.meta
    }

    /// 解码后的全文。
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 字符总数。
    #[must_use]
    pub fn char_count(&self) -> u64 {
        self.index.chars
    }

    /// 行数（空文本算 1 行）。
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// 字符偏移 → 字节偏移。
    ///
    /// `char_offset == char_count()` 时返回文本长度（即末尾位置）。
    #[must_use]
    pub fn char_to_byte(&self, char_offset: u64) -> Option<usize> {
        self.index.char_to_byte(&self.text, char_offset)
    }

    /// 字节偏移 → 字符偏移。非字符边界（切在多字节字符中间）返回 `None`。
    #[must_use]
    pub fn byte_to_char(&self, byte_offset: usize) -> Option<u64> {
        self.index.byte_to_char(&self.text, byte_offset)
    }

    /// 取一段字符区间对应的文本。
    #[must_use]
    pub fn text_range(&self, range: Range<u64>) -> Option<&str> {
        if range.start > range.end {
            return None;
        }
        let start = self.index.char_to_byte(&self.text, range.start)?;
        let end = self.index.char_to_byte(&self.text, range.end)?;
        self.text.get(start..end)
    }

    /// 某个字符偏移落在第几行（0 基）。
    #[must_use]
    pub fn line_index_at(&self, char_offset: u64) -> Option<usize> {
        if char_offset > self.char_count() {
            return None;
        }
        // 找最后一个 <= char_offset 的行起点
        match self.line_starts.binary_search(&char_offset) {
            Ok(exact) => Some(exact),
            Err(position) => Some(position.saturating_sub(1)),
        }
    }

    /// 第 `line` 行的字符区间（含行尾换行符）。
    #[must_use]
    pub fn line_char_range(&self, line: usize) -> Option<Range<u64>> {
        let start = *self.line_starts.get(line)?;
        let end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.char_count());
        Some(start..end)
    }

    /// 第 `line` 行的文本（已去掉行尾的 `\r` / `\n`）。
    #[must_use]
    pub fn line_text(&self, line: usize) -> Option<&str> {
        let range = self.line_char_range(line)?;
        let raw = self.text_range(range)?;
        Some(raw.trim_end_matches(['\r', '\n']))
    }

    /// 生成一个 TXT 的 [`Locator`]。
    ///
    /// 这里刻意**复用 `perch-model` 的构造器**：TXT / EPUB / PDF 三种格式的 Locator
    /// 必须是同一套类型与同一套语义，跨端同步才可能"一套代码"。
    #[must_use]
    pub fn locator(&self, char_offset: u64) -> Option<Locator> {
        if char_offset > self.char_count() {
            return None;
        }
        let byte_offset = self.char_to_byte(char_offset).map(|byte| byte as u64);
        Some(Locator::new_txt(
            char_offset,
            byte_offset,
            Some(self.char_count()),
        ))
    }

    /// 由 [`Locator`] 还原字符偏移。
    ///
    /// 按"精确度从高到低"依次尝试：字符偏移 → 字节偏移 → 全书进度。
    /// 这条回退链正是 Locator 设计里"多字段冗余"的意义所在——
    /// 跨端同步时某个字段缺失或被别的端写坏，仍能退化定位。
    #[must_use]
    pub fn char_offset_of_locator(&self, locator: &Locator) -> Option<u64> {
        if let Some(offset) = locator.perch.txt_offset {
            return (offset <= self.char_count()).then_some(offset);
        }
        if let Some(byte_offset) = locator.perch.txt_byte_offset {
            let byte_offset = usize::try_from(byte_offset).ok()?;
            return self.byte_to_char(byte_offset);
        }
        locator
            .locations
            .progression
            .map(|progression| (progression * self.char_count() as f64).round() as u64)
    }

    /// 把全文切成固定大小的字符区间（供全文索引分片使用）。
    ///
    /// `chunk_chars` 为 0 时返回空。
    #[must_use]
    pub fn chunk_ranges(&self, chunk_chars: u64) -> Vec<Range<u64>> {
        if chunk_chars == 0 {
            return Vec::new();
        }
        let total = self.char_count();
        let mut ranges = Vec::new();
        let mut start = 0u64;
        while start < total {
            let end = (start + chunk_chars).min(total);
            ranges.push(start..end);
            start = end;
        }
        ranges
    }

    /// 取局部上下文（供 Locator 的容错重定位字段使用）。
    ///
    /// 返回 `(before, highlight, after)`：以 `range` 为中心、各取 `context` 个字符。
    #[must_use]
    pub fn context_around(
        &self,
        range: Range<u64>,
        context: u64,
    ) -> Option<(String, String, String)> {
        let highlight = self.text_range(range.clone())?.to_owned();
        let before_start = range.start.saturating_sub(context);
        let after_end = (range.end + context).min(self.char_count());
        let before = self.text_range(before_start..range.start)?.to_owned();
        let after = self.text_range(range.end..after_end)?.to_owned();
        Some((before, highlight, after))
    }
}

impl std::fmt::Debug for TxtDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxtDocument")
            .field("encoding", &self.meta.encoding)
            .field("certain_encoding", &self.meta.certain_encoding)
            .field("chars", &self.meta.chars)
            .field("lines", &self.meta.lines)
            .finish()
    }
}

/// 抽样的"字符偏移 ↔ 字节偏移"索引。
///
/// 见模块文档的说明：不存每个字符的偏移，只每 [`SAMPLE_STRIDE`] 个字符存一个。
#[derive(Debug, Clone)]
struct CharIndex {
    /// `samples[k]` = 第 `k * SAMPLE_STRIDE` 个字符的字节偏移（一定是字符边界）。
    samples: Vec<usize>,
    chars: u64,
    bytes: usize,
}

impl CharIndex {
    fn build(text: &str) -> Self {
        let mut samples = vec![0usize];
        let mut chars: u64 = 0;
        for (byte_offset, _) in text.char_indices() {
            if chars > 0 && chars % SAMPLE_STRIDE == 0 {
                samples.push(byte_offset);
            }
            chars += 1;
        }
        Self {
            samples,
            chars,
            bytes: text.len(),
        }
    }

    fn char_to_byte(&self, text: &str, char_offset: u64) -> Option<usize> {
        if char_offset > self.chars {
            return None;
        }
        if char_offset == self.chars {
            return Some(self.bytes);
        }
        let sample = (char_offset / SAMPLE_STRIDE) as usize;
        let mut byte = *self.samples.get(sample)?;
        let mut remaining = char_offset - (sample as u64) * SAMPLE_STRIDE;
        let mut iter = text.get(byte..)?.char_indices();
        while remaining > 0 {
            let (_, character) = iter.next()?;
            byte += character.len_utf8();
            remaining -= 1;
        }
        Some(byte)
    }

    fn byte_to_char(&self, text: &str, byte_offset: usize) -> Option<u64> {
        if byte_offset > self.bytes {
            return None;
        }
        if byte_offset == self.bytes {
            return Some(self.chars);
        }
        if !text.is_char_boundary(byte_offset) {
            return None;
        }
        // 找到最后一个 <= byte_offset 的抽样点
        let sample = match self.samples.binary_search(&byte_offset) {
            Ok(exact) => exact,
            Err(position) => position.checked_sub(1)?,
        };
        let base_byte = *self.samples.get(sample)?;
        let base_char = (sample as u64) * SAMPLE_STRIDE;
        let extra = text.get(base_byte..byte_offset)?.chars().count() as u64;
        Some(base_char + extra)
    }
}

/// 每行起始的字符偏移。
fn build_line_starts(text: &str) -> Vec<u64> {
    let mut starts = vec![0u64];
    let mut chars: u64 = 0;
    for character in text.chars() {
        chars += 1;
        if character == '\n' {
            starts.push(chars);
        }
    }
    starts
}

/// 编码探测与解码的结果。
struct Detected {
    text: std::borrow::Cow<'static, str>,
    /// 编码名（`encoding_rs` 的名字）
    encoding: &'static str,
    /// 编码是否确定（BOM / 合法 UTF-8 / 用户指定）——否则是探测器的猜测。
    certain: bool,
    /// 解码时是否出现过非法序列（出现即说明编码可能选错了）。
    had_errors: bool,
}

/// 探测编码并解码。**只解码一次**（大文件上解码是主要开销，不能重复）。
fn detect_and_decode(bytes: &[u8]) -> Detected {
    // 1) BOM 是确定的
    if let Some((encoding, _)) = Encoding::for_bom(bytes) {
        let (text, _, had_errors) = encoding.decode(bytes);
        return Detected {
            text: std::borrow::Cow::Owned(text.into_owned()),
            encoding: encoding.name(),
            certain: true,
            had_errors,
        };
    }

    // 2) 合法 UTF-8 基本可以确定（GBK 中文很难恰好构成合法 UTF-8）
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Detected {
            text: std::borrow::Cow::Owned(text.to_owned()),
            encoding: encoding_rs::UTF_8.name(),
            certain: true,
            had_errors: false,
        };
    }

    // 3) 只能猜。chardetng 对长文本比短文本可靠得多。
    //
    // - UTF-8 参与候选（本地文件最常见的就是 UTF-8）
    // - ISO-2022-JP 不参与候选：它是给邮件用的古老编码，对中文书没有意义，
    //   关掉可以少一个"猜错"的来源（chardetng 文档也建议在可疑场合 Deny）
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    detector.feed(bytes, true);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Allow);
    let (text, _, had_errors) = encoding.decode(bytes);
    Detected {
        text: std::borrow::Cow::Owned(text.into_owned()),
        encoding: encoding.name(),
        certain: false,
        had_errors,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";
    /// "中文" 的 GBK 编码（手工核对过的标准值，不是用同一个库编出来的）
    const ZHONG_WEN_GBK: &[u8] = &[0xD6, 0xD0, 0xCE, 0xC4];
    /// "中文" 的 Big5 编码
    const ZHONG_WEN_BIG5: &[u8] = &[0xA4, 0xA4, 0xA4, 0xE5];

    #[test]
    fn decodes_utf8_without_bom() {
        let doc = TxtDocument::from_bytes("你好，世界".as_bytes());
        assert_eq!(doc.text(), "你好，世界");
        assert_eq!(doc.meta().encoding, "UTF-8");
        assert!(doc.meta().certain_encoding);
        assert!(!doc.meta().had_decode_errors);
    }

    #[test]
    fn decodes_utf8_with_bom_and_strips_it() {
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice("带 BOM 的文本".as_bytes());
        let doc = TxtDocument::from_bytes(&bytes);
        assert_eq!(doc.text(), "带 BOM 的文本", "BOM 不应出现在正文里");
        assert_eq!(doc.meta().encoding, "UTF-8");
        assert!(doc.meta().certain_encoding);
    }

    #[test]
    fn decodes_explicit_gbk_and_big5() {
        // 用显式编码路径验证"解码本身正确"（这条不依赖探测的准确性）
        let gbk = TxtDocument::with_encoding(ZHONG_WEN_GBK, "GBK").unwrap();
        assert_eq!(gbk.text(), "中文");

        let big5 = TxtDocument::with_encoding(ZHONG_WEN_BIG5, "Big5").unwrap();
        assert_eq!(big5.text(), "中文");
    }

    #[test]
    fn detects_gbk_for_long_chinese_text() {
        // 短样本探测本就不可靠，因此用长文本验证探测能力
        let original = "中文测试，这是一段足够长的文本用来让编码探测器有足够的依据。".repeat(20);
        let (gbk_bytes, _, _) = encoding_rs::GBK.encode(&original);

        let doc = TxtDocument::from_bytes(&gbk_bytes);
        assert_eq!(doc.text(), original, "探测出的编码必须能正确解码");
        assert!(!doc.meta().certain_encoding, "猜出来的编码不应被标为确定");
        assert!(
            matches!(doc.meta().encoding, "GBK" | "GB18030"),
            "应落在 GB 系编码，实际 {}",
            doc.meta().encoding
        );
    }

    #[test]
    fn explicit_encoding_overrides_detection() {
        // 即使探测会猜错，显式指定必须生效（对应 UI 的"换个编码重读"）
        let doc = TxtDocument::with_encoding(ZHONG_WEN_GBK, "GBK").unwrap();
        assert_eq!(doc.meta().encoding, "GBK");
        assert!(doc.meta().certain_encoding, "显式指定即为确定");

        assert!(TxtDocument::with_encoding(ZHONG_WEN_GBK, "不存在的编码").is_none());
    }

    #[test]
    fn char_and_byte_offsets_round_trip_with_cjk() {
        let doc = TxtDocument::from_bytes("中文abc中文".as_bytes());
        assert_eq!(doc.char_count(), 7);

        for char_offset in 0..=doc.char_count() {
            let byte = doc.char_to_byte(char_offset).unwrap();
            assert_eq!(doc.byte_to_char(byte), Some(char_offset));
        }
        // 逐字符核对字节位置：中(3) 文(6) a(7) b(8) c(9) 中(12) 文(15)
        assert_eq!(doc.char_to_byte(1), Some(3));
        assert_eq!(doc.char_to_byte(2), Some(6));
        assert_eq!(doc.char_to_byte(3), Some(7), "中文a = 3+3+1");
        assert_eq!(doc.char_to_byte(4), Some(8));
        assert_eq!(doc.char_to_byte(5), Some(9));
        // 末尾位置 == 字节长度
        assert_eq!(doc.char_to_byte(doc.char_count()), Some(doc.text().len()));
    }

    #[test]
    fn byte_to_char_rejects_non_boundary() {
        let doc = TxtDocument::from_bytes("中".as_bytes());
        // "中" 的 UTF-8 是 3 字节，第 1、2 字节不是字符边界
        assert_eq!(doc.byte_to_char(1), None);
        assert_eq!(doc.byte_to_char(2), None);
        assert_eq!(doc.byte_to_char(0), Some(0));
        assert_eq!(doc.byte_to_char(3), Some(1));
    }

    #[test]
    fn sampled_index_is_correct_across_stride_boundaries() {
        // 这是抽样索引最容易出错的地方：跨越多个抽样点
        let text = "汉".repeat((SAMPLE_STRIDE * 3 + 7) as usize);
        let doc = TxtDocument::from_bytes(text.as_bytes());
        assert_eq!(doc.char_count(), SAMPLE_STRIDE * 3 + 7);

        // 逐字符验证（含抽样点前后）
        for char_offset in 0..=doc.char_count() {
            let byte = doc.char_to_byte(char_offset).unwrap();
            assert_eq!(byte, char_offset as usize * 3, "char_offset={char_offset}");
            assert_eq!(doc.byte_to_char(byte), Some(char_offset));
        }

        // 混合宽度文本：抽样点必须落在字符边界上
        let mixed = "a汉".repeat((SAMPLE_STRIDE * 2 + 3) as usize);
        let doc = TxtDocument::from_bytes(mixed.as_bytes());
        for char_offset in 0..=doc.char_count() {
            let byte = doc.char_to_byte(char_offset).unwrap();
            assert_eq!(doc.byte_to_char(byte), Some(char_offset));
        }
    }

    #[test]
    fn out_of_range_offsets_return_none() {
        let doc = TxtDocument::from_bytes("abc".as_bytes());
        assert!(doc.char_to_byte(4).is_none());
        assert!(doc.byte_to_char(4).is_none());
        assert!(doc.line_index_at(4).is_none());
        // 用变量而不是字面量：clippy 对字面量范围能静态判定"恒空"并报错，
        // 而这里要测的恰恰是"运行时传入反向区间时的行为"
        let (start, end) = (2u64, 1u64);
        assert!(doc.text_range(start..end).is_none());
    }

    #[test]
    fn line_index_handles_lf_and_crlf() {
        let doc = TxtDocument::from_bytes("第一行\n第二行\r\n第三行".as_bytes());
        assert_eq!(doc.line_count(), 3);
        assert_eq!(doc.line_text(0), Some("第一行"));
        assert_eq!(doc.line_text(1), Some("第二行"), "CRLF 的 \\r 要去掉");
        assert_eq!(doc.line_text(2), Some("第三行"));

        // 行区间（含行尾换行符）：
        // 第一行\n = 3+1 = 4 → 0..4
        // 第二行\r\n = 3+1+1 = 5 → 4..9
        // 第三行 = 3 → 9..12
        assert_eq!(doc.line_char_range(0), Some(0..4));
        assert_eq!(doc.line_char_range(1), Some(4..9));
        assert_eq!(doc.line_char_range(2), Some(9..12));
        assert!(doc.line_char_range(3).is_none());

        // 反查：某个偏移在第几行
        assert_eq!(doc.line_index_at(0), Some(0));
        assert_eq!(doc.line_index_at(3), Some(0));
        assert_eq!(doc.line_index_at(4), Some(1));
        assert_eq!(doc.line_index_at(11), Some(2));
        // 总字符数 = 末尾位置，仍属于最后一行
        assert_eq!(doc.line_index_at(12), Some(2));
    }

    #[test]
    fn trailing_newline_creates_empty_last_line() {
        let doc = TxtDocument::from_bytes("一行\n".as_bytes());
        assert_eq!(doc.line_count(), 2);
        assert_eq!(doc.line_text(1), Some(""));
    }

    #[test]
    fn empty_document_is_usable() {
        let doc = TxtDocument::from_bytes(b"");
        assert_eq!(doc.char_count(), 0);
        assert_eq!(doc.line_count(), 1);
        assert_eq!(doc.line_text(0), Some(""));
        assert_eq!(doc.char_to_byte(0), Some(0));
        assert!(doc.byte_to_char(0).is_some());
        assert!(doc.chunk_ranges(100).is_empty());
    }

    #[test]
    fn locator_round_trips_through_model() {
        let doc = TxtDocument::from_bytes("第一章\n正文内容。".as_bytes());

        let locator = doc.locator(5).unwrap();
        // 用的是 perch-model 的构造器：格式标记与进度都应被填好
        assert_eq!(locator.format(), Some(perch_model::Format::Txt));
        assert_eq!(locator.perch.txt_offset, Some(5));
        assert_eq!(
            locator.perch.txt_byte_offset,
            doc.char_to_byte(5).map(|b| b as u64)
        );
        locator.validate().unwrap();

        // 回到字符偏移
        assert_eq!(doc.char_offset_of_locator(&locator), Some(5));
    }

    #[test]
    fn locator_resolution_falls_back_when_fields_are_missing() {
        let doc = TxtDocument::from_bytes("第一章\n正文内容。".as_bytes());

        // 只留字节偏移
        let mut locator = doc.locator(5).unwrap();
        locator.perch.txt_offset = None;
        assert_eq!(doc.char_offset_of_locator(&locator), Some(5));

        // 只留进度：允许有误差，但必须能给出一个位置
        let mut locator = doc.locator(4).unwrap();
        locator.perch.txt_offset = None;
        locator.perch.txt_byte_offset = None;
        let offset = doc.char_offset_of_locator(&locator).unwrap();
        assert!(offset <= doc.char_count());

        // 什么都没有
        locator.locations.progression = None;
        assert_eq!(doc.char_offset_of_locator(&locator), None);
    }

    #[test]
    fn chunk_ranges_cover_whole_text() {
        let doc = TxtDocument::from_bytes("a".repeat(25).as_bytes());
        let chunks = doc.chunk_ranges(10);
        assert_eq!(chunks, vec![0..10, 10..20, 20..25]);

        // 无缝无重叠
        let mut expected_start = 0;
        for chunk in &chunks {
            assert_eq!(chunk.start, expected_start);
            expected_start = chunk.end;
        }
        assert_eq!(expected_start, doc.char_count());

        assert!(doc.chunk_ranges(0).is_empty());
    }

    #[test]
    fn text_range_extracts_by_char_offsets() {
        let doc = TxtDocument::from_bytes("中文abc".as_bytes());
        assert_eq!(doc.text_range(0..2), Some("中文"));
        assert_eq!(doc.text_range(2..5), Some("abc"));
        assert_eq!(doc.text_range(0..0), Some(""));
        assert_eq!(doc.text_range(0..5), Some("中文abc"));
    }

    #[test]
    fn context_around_gives_anchor_context() {
        let doc = TxtDocument::from_bytes("前面的话高亮内容后面的话".as_bytes());
        // "高亮内容" 从第 4 个字符开始，共 4 个字符
        let (before, highlight, after) = doc.context_around(4..8, 3).unwrap();
        assert_eq!(highlight, "高亮内容");
        assert_eq!(before, "面的话");
        assert_eq!(after, "后面的");

        // 边界处不应越界
        let (before, _, _) = doc.context_around(0..2, 10).unwrap();
        assert_eq!(before, "");
    }
}
