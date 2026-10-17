//! 全文索引：CJK bigram 分词 + 倒排（token → 分片）。
//!
//! 对应导入管线的第三阶段（延迟索引），目标见 `docs/01-里程碑拆解.md` M3。
//!
//! # 为什么要自己写而不是用 tantivy
//!
//! - **体积**：移动端装不下桌面级全文引擎
//! - **中文分词**：tantivy 默认按空白切词，中文会整段变成一个词。
//!   而中文分词需要词典（`jieba` 词典约 10MB），我们选择**不需要词典的 CJK bigram**
//! - **与 Locator 对齐**：搜索命中必须能变成 `Locator` 才能跳转与高亮，
//!   自有索引能直接记录字符偏移
//!
//! # 核心设计：token → 分片，而不是 token → 每个位置
//!
//! 一本书 100 万字，若为每个出现位置都存一个偏移，索引会比正文还大。
//! 所以倒排记录的是**分片编号**（默认 4096 字符一片）：
//!
//! ```text
//! "中文"  → [0, 3, 7]          出现过的分片
//! "测试"  → [0, 7]
//! ```
//!
//! 查询时先对分片求交（很快），再**只在命中的分片里做精确扫描**得到确切偏移。
//! 求交前会把各 token 的分片列表按查询长度**向前扩展**，否则跨分片的短语会被漏掉
//! （详见 `search` 里的注释）。
//! 这是移动端常见的取舍：索引小一个量级，代价是命中后要扫几个分片。
//!
//! # 分词规则（不需要词典）
//!
//! - **CJK**：单字 + 相邻二元组。`中文测试` → `中` `中文` `文` `文测` `测` `测试` `试`。
//!   保留单字是为了让单字查询也能命中；代价是倒排条目约翻倍。
//! - **拉丁/数字**：按非字母数字切分并转小写，整词入索引。
//!
//! # 查询语义（重要）
//!
//! **查询被当作一个字面量短语做匹配**（ASCII 大小写不敏感）。
//!
//! token 的 AND 只用来**预筛分片**，不改变匹配语义：
//! 若短语在正文里出现，它的全部 token 必然都在同一分片，所以预筛是安全的。
//!
//! 这意味着：
//! - `"中文测试"` 命中正文里的同样子串 ✓
//! - `"苹果 香蕉"`（带空格）只会命中**正好写成这样**的文字；
//!   正文里写作"苹果和香蕉"时**不命中** —— 这是"短语搜索"的应有行为
//! - 跨标点、跨词形的模糊匹配**不支持**（有意为之：宁可少也不要错）
//!
//! 若将来需要真正的"多词 AND"（词可以不相邻），应当在 `search` 之外另加一个
//! `search_all_words`，而不要悄悄改掉短语语义。
//!
//! # 已知限制
//!
//! - 倒排用 `HashMap<String, Vec<u32>>`，是按书构建的。整库（1000 本）不能同时驻留内存，
//!   应逐本建好后落盘（持久化格式尚未实现，见下一步）
//! - 未实现前缀/模糊匹配

use std::collections::HashMap;
use std::ops::Range;

/// 默认分片大小（字符数）。4096 大约是手机一屏到几屏的文字量。
pub const DEFAULT_CHUNK_CHARS: u64 = 4096;

/// 摘要窗口：命中处前后各取多少字符作为 snippet。
const SNIPPET_CONTEXT: u64 = 24;

/// 一个分词结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    /// 词面（CJK 为单字或二元组；拉丁为小写整词）。
    pub text: &'a str,
    /// 在原文中的**字符**偏移。
    pub char_offset: u64,
}

/// 一条搜索命中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// 命中起点（字符偏移）。
    pub char_start: u64,
    /// 命中终点（字符偏移，开区间）。
    pub char_end: u64,
    /// 所在分片编号。
    pub chunk: u32,
    /// 命中处前后文，便于列表展示。
    pub snippet: String,
}

/// 索引统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// 分片数。
    pub chunks: usize,
    /// 倒排里不同 token 的数量。
    pub tokens: usize,
    /// 倒排条目总数（各 token 的分片数之和）。
    pub postings: usize,
    /// 正文字符数。
    pub chars: u64,
}

/// 判断是否为 CJK 字符（需要 bigram 分词的那些）。
///
/// 覆盖基本汉字、扩展 A、兼容汉字，以及假名与谚文（它们的检索需求与汉字一致）。
fn is_cjk(character: char) -> bool {
    matches!(character as u32,
        0x3040..=0x30FF        // 平假名 / 片假名
        | 0x3400..=0x4DBF      // 扩展 A
        | 0x4E00..=0x9FFF      // 基本汉字
        | 0xAC00..=0xD7AF      // 谚文音节
        | 0xF900..=0xFAFF      // 兼容汉字
        | 0x20000..=0x2A6DF    // 扩展 B
    )
}

/// 是否为"词"字符（拉丁字母、数字、以及非 CJK 的字母）。
fn is_word(character: char) -> bool {
    character.is_alphanumeric() && !is_cjk(character)
}

/// 遍历文本中的所有 token。
///
/// 用回调而不是返回 `Vec`：建索引时 token 数量可达数十万，避免中间分配。
pub fn for_each_token<'a>(text: &'a str, mut visit: impl FnMut(Token<'a>)) {
    // 当前拉丁词的起点与累积
    let mut word_start: Option<(u64, usize)> = None; // (字符偏移, 字节偏移)

    let flush_word = |word_start: &mut Option<(u64, usize)>,
                      visit: &mut dyn FnMut(Token<'a>),
                      end_byte: usize| {
        if let Some((char_offset, start_byte)) = word_start.take() {
            if let Some(word) = text.get(start_byte..end_byte) {
                if !word.is_empty() {
                    // 拉丁词统一小写入索引（查询侧同样处理）
                    visit(Token {
                        text: word,
                        char_offset,
                    });
                }
            }
        }
    };

    // CJK 连续段的缓冲：需要相邻字组 bigram，因此保留上一个字
    let mut previous_cjk: Option<(char, u64, usize)> = None;

    // char_offset 由 zip 提供，避免手动维护循环计数器
    for (char_offset, (byte_index, character)) in (0u64..).zip(text.char_indices()) {
        if is_cjk(character) {
            flush_word(&mut word_start, &mut visit, byte_index);
            // 与上一个 CJK 字组成 bigram
            if let Some((_previous, previous_offset, previous_byte)) = previous_cjk {
                if let Some(bigram) = text.get(previous_byte..byte_index + character.len_utf8()) {
                    visit(Token {
                        text: bigram,
                        char_offset: previous_offset,
                    });
                }
            }
            // 单字也入索引，保证单字查询能命中
            if let Some(single) = text.get(byte_index..byte_index + character.len_utf8()) {
                visit(Token {
                    text: single,
                    char_offset,
                });
            }
            previous_cjk = Some((character, char_offset, byte_index));
        } else if is_word(character) {
            previous_cjk = None;
            if word_start.is_none() {
                word_start = Some((char_offset, byte_index));
            }
        } else {
            previous_cjk = None;
            flush_word(&mut word_start, &mut visit, byte_index);
        }
    }
    flush_word(&mut word_start, &mut visit, text.len());
}

/// 反转义 latin 词的辅助：查询侧也要小写，才能与索引侧匹配。
fn normalize_query_word(word: &str) -> String {
    word.to_lowercase()
}

/// 一本书的全文索引。
pub struct Index {
    text: String,
    chunk_chars: u64,
    chunks: Vec<Range<u64>>,
    /// token → 出现过的分片编号（升序、去重）。
    postings: HashMap<String, Vec<u32>>,
}

impl Index {
    /// 建立索引。`chunk_chars` 为 0 时退回 [`DEFAULT_CHUNK_CHARS`]。
    #[must_use]
    pub fn build(text: impl Into<String>, chunk_chars: u64) -> Self {
        let text = text.into();
        let chunk_chars = if chunk_chars == 0 {
            DEFAULT_CHUNK_CHARS
        } else {
            chunk_chars
        };
        let char_count = text.chars().count() as u64;

        let mut chunks = Vec::new();
        let mut start = 0u64;
        while start < char_count {
            let end = (start + chunk_chars).min(char_count);
            chunks.push(start..end);
            start = end;
        }

        let mut postings: HashMap<String, Vec<u32>> = HashMap::new();
        // 拉丁词需要小写后再入索引，因此可能有临时分配
        for_each_token(&text, |token| {
            let chunk = (token.char_offset / chunk_chars) as u32;
            let key = if token.text.chars().all(|character| !is_cjk(character)) {
                normalize_query_word(token.text)
            } else {
                token.text.to_owned()
            };
            let entry = postings.entry(key).or_default();
            // 扫描顺序保证分片编号递增，只需比较最后一个
            if entry.last() != Some(&chunk) {
                entry.push(chunk);
            }
        });

        Self {
            text,
            chunk_chars,
            chunks,
            postings,
        }
    }

    /// 正文字符数。
    #[must_use]
    pub fn char_count(&self) -> u64 {
        self.text.chars().count() as u64
    }

    /// 分片大小。
    #[must_use]
    pub fn chunk_chars(&self) -> u64 {
        self.chunk_chars
    }

    /// 统计信息。
    #[must_use]
    pub fn stats(&self) -> Stats {
        Stats {
            chunks: self.chunks.len(),
            tokens: self.postings.len(),
            postings: self.postings.values().map(Vec::len).sum(),
            chars: self.char_count(),
        }
    }

    /// 搜索。多个词之间是 **AND** 关系。
    ///
    /// 返回按字符偏移升序排列的命中列表。
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<Hit> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }

        // 1) 查询分词（与建索引用同一套规则，保证能对上）
        let mut tokens: Vec<String> = Vec::new();
        for_each_token(trimmed, |token| {
            let key = if token.text.chars().all(|character| !is_cjk(character)) {
                normalize_query_word(token.text)
            } else {
                token.text.to_owned()
            };
            tokens.push(key);
        });
        if tokens.is_empty() {
            return Vec::new();
        }

        // 2) 分片求交
        //
        // 注意：短语可能**跨分片**，此时它的 token 会落在不同分片里（比如
        // "中文测试" 的 中/中文/文/文测 在分片 N，而 测/测试/试 在分片 N+1）。
        // 如果直接拿各 token 的分片列表求交，会把这种命中整个漏掉。
        // 因此先把每个 token 的分片列表**向前扩展**若干片，扩展量取
        // "一个起点最早落在分片开头的匹配，最多能伸到下一个分片的第几片"。
        let shift = (self.chunk_chars + needle_for_shift(trimmed) - 2) / self.chunk_chars;
        let mut candidates: Vec<u32> = Vec::new();
        for (position, token) in tokens.iter().enumerate() {
            let Some(ids) = self.postings.get(token) else {
                return Vec::new();
            };
            let expanded = expand_down(ids, shift);
            candidates = if position == 0 {
                expanded
            } else {
                intersect(&candidates, &expanded)
            };
            if candidates.is_empty() {
                return Vec::new();
            }
        }

        // 3) 在候选分片内做精确的字面量扫描
        let needle: Vec<char> = trimmed.chars().collect();
        let mut hits = Vec::new();
        for chunk_id in candidates {
            let Some(range) = self.chunks.get(chunk_id as usize) else {
                continue;
            };
            // 往后多看 needle 长度，覆盖跨分片的命中
            let scan_end = (range.end + needle.len() as u64).min(self.char_count());
            let Some(haystack) = self.text_range(range.start..scan_end) else {
                continue;
            };

            let haystack_chars: Vec<char> = haystack.chars().collect();
            if needle.len() > haystack_chars.len() {
                continue;
            }
            for index in 0..=(haystack_chars.len() - needle.len()) {
                let matched = haystack_chars[index..index + needle.len()]
                    .iter()
                    .zip(&needle)
                    .all(|(hay, query)| chars_equal(*hay, *query));
                if !matched {
                    continue;
                }
                let char_start = range.start + index as u64;
                // 命中起点必须落在本分片内，避免跨分片重复上报
                if char_start >= range.end {
                    continue;
                }
                let char_end = char_start + needle.len() as u64;
                hits.push(Hit {
                    char_start,
                    char_end,
                    chunk: chunk_id,
                    snippet: self.snippet(char_start, char_end),
                });
            }
        }
        hits.sort_by_key(|hit| hit.char_start);
        hits
    }

    /// 取一段字符区间的文本。
    fn text_range(&self, range: Range<u64>) -> Option<&str> {
        let start = self.byte_of_char(range.start)?;
        let end = self.byte_of_char(range.end)?;
        self.text.get(start..end)
    }

    /// 字符偏移 → 字节偏移（线性扫描；索引侧只用于少量取样，够用）。
    fn byte_of_char(&self, char_offset: u64) -> Option<usize> {
        if char_offset == 0 {
            return Some(0);
        }
        let mut count = 0u64;
        for (byte_index, _) in self.text.char_indices() {
            if count == char_offset {
                return Some(byte_index);
            }
            count += 1;
        }
        if count == char_offset {
            Some(self.text.len())
        } else {
            None
        }
    }

    /// 命中处的上下文片段。
    fn snippet(&self, char_start: u64, char_end: u64) -> String {
        let total = self.char_count();
        let from = char_start.saturating_sub(SNIPPET_CONTEXT);
        let to = (char_end + SNIPPET_CONTEXT).min(total);
        let mut snippet = String::new();
        if from > 0 {
            snippet.push('…');
        }
        if let Some(text) = self.text_range(from..to) {
            snippet.push_str(text);
        }
        if to < total {
            snippet.push('…');
        }
        snippet
    }
}

impl std::fmt::Debug for Index {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stats = self.stats();
        formatter
            .debug_struct("Index")
            .field("chunks", &stats.chunks)
            .field("tokens", &stats.tokens)
            .field("chars", &stats.chars)
            .finish()
    }
}

/// ASCII 大小写不敏感的字符比较（CJK 与其它字符走精确比较）。
fn chars_equal(left: char, right: char) -> bool {
    left == right || left.eq_ignore_ascii_case(&right)
}

/// 查询的字符长度（用于算跨分片扩展量）。
fn needle_for_shift(query: &str) -> u64 {
    query.chars().count().max(1) as u64
}

/// 把升序的分片编号列表**向前**扩展 `shift` 片并去重。
///
/// 例如 `[5]`、`shift=1` → `[4, 5]`：表示"命中可能起始于分片 4 而延伸到 5"。
fn expand_down(ids: &[u32], shift: u64) -> Vec<u32> {
    if shift == 0 {
        return ids.to_vec();
    }
    let mut expanded: Vec<u32> = Vec::with_capacity(ids.len() * 2);
    for &id in ids {
        let low = (id as u64).saturating_sub(shift) as u32;
        for candidate in low..=id {
            if expanded.last() != Some(&candidate) {
                expanded.push(candidate);
            }
        }
    }
    expanded
}

/// 两个升序 u32 列表求交。
fn intersect(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut result = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                result.push(left[i]);
                i += 1;
                j += 1;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn token_texts(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for_each_token(text, |token| out.push(token.text.to_owned()));
        out
    }

    #[test]
    fn cjk_yields_unigrams_and_bigrams() {
        assert_eq!(
            token_texts("中文测"),
            vec!["中", "中文", "文", "文测", "测"]
        );
    }

    #[test]
    fn latin_words_are_split_and_lowercased_by_index() {
        assert_eq!(token_texts("Hello,World 42"), vec!["Hello", "World", "42"]);
        let index = Index::build("Hello World", 100);
        // 索引里存的是小写
        assert!(index.postings.contains_key("hello"));
        assert!(index.postings.contains_key("world"));
    }

    #[test]
    fn mixed_cjk_and_latin() {
        assert_eq!(token_texts("中文abc"), vec!["中", "中文", "文", "abc"]);
    }

    #[test]
    fn token_offsets_point_at_the_source() {
        let text = "中文";
        let mut offsets = Vec::new();
        for_each_token(text, |token| offsets.push((token.text, token.char_offset)));
        assert_eq!(offsets, vec![("中", 0), ("中文", 0), ("文", 1)]);
    }

    #[test]
    fn chunk_count_follows_chunk_size() {
        let index = Index::build("a".repeat(25), 10);
        assert_eq!(index.stats().chunks, 3);
        assert_eq!(index.chunk_chars(), 10);

        // 0 退回默认值
        let fallback = Index::build("abc", 0);
        assert_eq!(fallback.chunk_chars(), DEFAULT_CHUNK_CHARS);
    }

    #[test]
    fn finds_cjk_substring_with_exact_offsets() {
        let text = "前面的内容。中文测试在这里。后面的内容。";
        let index = Index::build(text, DEFAULT_CHUNK_CHARS);
        let hits = index.search("中文测试");
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        // 命中区间取出来必须正好是查询串
        let matched: String = text
            .chars()
            .skip(hit.char_start as usize)
            .take((hit.char_end - hit.char_start) as usize)
            .collect();
        assert_eq!(matched, "中文测试");
        assert!(hit.snippet.contains("中文测试"));
    }

    #[test]
    fn finds_single_cjk_character() {
        let index = Index::build("中文测试", DEFAULT_CHUNK_CHARS);
        let hits = index.search("测");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].char_start, 2);
    }

    #[test]
    fn latin_search_is_case_insensitive() {
        let index = Index::build("Hello World, hello rust", DEFAULT_CHUNK_CHARS);
        let hits = index.search("hello");
        assert_eq!(hits.len(), 2, "大小写不同的两处都要命中");
        assert_eq!(hits[0].char_start, 0);
        assert_eq!(hits[1].char_start, 13);
    }

    #[test]
    fn query_is_a_literal_phrase_not_a_word_and() {
        // 正文写作"苹果和香蕉"，查询带空格 → 短语不存在，不应命中
        let index = Index::build("苹果和香蕉都很好吃", DEFAULT_CHUNK_CHARS);
        assert!(index.search("苹果 香蕉").is_empty());

        // 正文里确实有"苹果 香蕉"这种写法时才命中
        let spaced = Index::build("苹果 香蕉 都很好吃", DEFAULT_CHUNK_CHARS);
        assert_eq!(spaced.search("苹果 香蕉").len(), 1);

        // 不带空格的连续短语正常命中
        let index = Index::build("苹果和香蕉都很好吃", DEFAULT_CHUNK_CHARS);
        assert_eq!(index.search("苹果和香蕉").len(), 1);
    }

    #[test]
    fn phrase_must_be_contiguous_not_just_all_tokens_present() {
        // token 都在、但短语不连续 → 不应命中。
        // 这条走的是"验证阶段否掉候选"的路径（预筛只保证 token 都出现过）。
        let latin = Index::build("hello world", DEFAULT_CHUNK_CHARS);
        assert_eq!(latin.search("hello world").len(), 1, "短语存在应命中");
        assert!(
            latin.search("world hello").is_empty(),
            "两个词都在，但短语不连续，不应命中"
        );
    }

    #[test]
    fn cross_chunk_phrase_is_found_at_any_chunk_size() {
        // 同一段文字在不同分片大小下都必须找到同样的命中
        let text = "苹果和香蕉都很好吃";
        for chunk in [1u64, 2, 3, 4, 5, 16, DEFAULT_CHUNK_CHARS] {
            let index = Index::build(text, chunk);
            let hits = index.search("苹果和香蕉");
            assert_eq!(hits.len(), 1, "chunk={chunk} 时应命中一次");
            assert_eq!(hits[0].char_start, 0, "chunk={chunk}");
            assert_eq!(hits[0].char_end, 5, "chunk={chunk}");
        }
    }

    #[test]
    fn no_match_returns_empty() {
        let index = Index::build("中文测试", DEFAULT_CHUNK_CHARS);
        assert!(index.search("英文").is_empty());
        assert!(index.search("").is_empty());
        assert!(index.search("   ").is_empty());
        // 纯标点：分词后没有任何 token
        assert!(index.search("！！！").is_empty());
    }

    #[test]
    fn match_spanning_a_chunk_boundary_is_found_once() {
        // 让"中文测试"跨过 4 字符的分片边界
        let text = "一二中文测试三四";
        let index = Index::build(text, 4);
        // 8 个字符、每片 4 个 → 2 片
        assert_eq!(index.stats().chunks, 2);

        let hits = index.search("中文测试");
        assert_eq!(hits.len(), 1, "跨分片的命中不能重复上报，也不能漏");
        assert_eq!(hits[0].char_start, 2);
        assert_eq!(hits[0].char_end, 6);
    }

    #[test]
    fn snippet_is_clipped_and_marked() {
        let text = format!("{}关键字{}", "前".repeat(50), "后".repeat(50));
        let index = Index::build(text, DEFAULT_CHUNK_CHARS);
        let hits = index.search("关键字");
        assert_eq!(hits.len(), 1);
        let snippet = &hits[0].snippet;
        assert!(snippet.contains("关键字"));
        assert!(snippet.starts_with('…'), "截断时应有省略号");
        assert!(snippet.ends_with('…'));
    }

    #[test]
    fn stats_are_consistent() {
        let index = Index::build("中文测试 abc", DEFAULT_CHUNK_CHARS);
        let stats = index.stats();
        assert_eq!(stats.chunks, 1);
        assert_eq!(stats.chars, 8);
        assert!(stats.tokens > 0);
        assert!(stats.postings >= stats.tokens, "每个 token 至少一个分片");
    }

    #[test]
    fn repeated_token_in_phrase_is_fine() {
        // 短语里同一个 token 出现多次（"中文中文"里"中文"出现两次）
        // 不应把候选集算空
        let index = Index::build("中文中文", DEFAULT_CHUNK_CHARS);
        assert_eq!(index.search("中文中文").len(), 1);
    }

    #[test]
    fn intersect_helper_works() {
        assert_eq!(intersect(&[1, 3, 5], &[3, 4, 5]), vec![3, 5]);
        assert!(intersect(&[1, 2], &[3, 4]).is_empty());
        assert!(intersect(&[], &[1]).is_empty());
    }
}
