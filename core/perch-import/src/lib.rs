//! 导入管线**阶段①（登记）**：文件指纹、去重、登记计划。
//!
//! 目标见 `项目目标.txt`：**1000 本感知上架 < 10 秒**。
//!
//! # 阶段①为什么能这么快
//!
//! 整条导入被拆成三个阶段（见 `docs/02-架构设计.md` §六）：
//!
//! | 阶段 | 做什么 | 何时 |
//! |---|---|---|
//! | **① 登记** | 算指纹 + 落盘 + 插一行 `state=registered` | 前台，用户点导入后立刻 |
//! | ② 精修 | 解析元数据 / 封面 / 目录 / 字数 | 后台 |
//! | ③ 索引 | 建立全文倒排 | 后台，可延迟 |
//!
//! **① 阶段绝不打开文件内容解析**（除首尾各 64KB 用于指纹）。这是"1000 本 <10s"
//! 能成立的根本原因，也是后续所有性能回归的红线。
//!
//! # 指纹为什么不整文件哈希
//!
//! 1000 本 × 平均 2MB = 2GB，整本哈希光 I/O 就要几十秒。改为
//! **`(文件大小, 首 64KB 摘要, 尾 64KB 摘要)`** 合成一个 SHA-256：
//!
//! - 电子书几乎不会只在中间的某段不同而头尾完全相同
//! - 小文件（< 128KB）首尾会重叠，这没关系，仍然只是"多算了一点"
//!
//! **代价**（如实说明）：两本仅中间内容不同的书会被判为同一本。这对"同一次扫描里重复出现的同一文件"
//! 这个主要场景完全够用；若将来需要更强的判重，可以升级成"头尾 + 随机抽 3 段"。
//!
//! # 职责边界
//!
//! 本 crate **不做 I/O**：读文件、复制文件、写数据库都是平台层（Dart/Swift）的事。
//! 这里只做纯计算：**给定字节算指纹**、**给定候选清单算登记计划**。
//! 这样整条管线逻辑都能在 Linux 上测试。

use std::collections::HashSet;
use std::fmt;

use perch_model::{Book, BookId, Format};
use sha2::{Digest, Sha256};

/// 指纹采样窗口：首尾各取多少字节。
pub const SAMPLE_BYTES: usize = 64 * 1024;

/// 文件指纹（32 字节 SHA-256）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// 由文件大小与首尾字节片段计算指纹。
    ///
    /// `head` 应是文件前 [`SAMPLE_BYTES`] 字节（不足则全部），
    /// `tail` 应是文件后 [`SAMPLE_BYTES`] 字节（不足则全部）。
    #[must_use]
    pub fn compute(size: u64, head: &[u8], tail: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        // 大小参与哈希：否则"头尾相同但长度不同"的两个文件会撞在一起
        hasher.update(size.to_le_bytes());
        // 两个片段之间加分隔与各自长度，避免 ("ab","c") 与 ("a","bc") 撞车
        hasher.update((head.len() as u64).to_le_bytes());
        hasher.update(head);
        hasher.update((tail.len() as u64).to_le_bytes());
        hasher.update(tail);
        Self(hasher.finalize().into())
    }

    /// 由十六进制字符串还原。
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (index, chunk) in text.as_bytes().chunks_exact(2).enumerate() {
            let high = hex_value(chunk[0])?;
            let low = hex_value(chunk[1])?;
            bytes[index] = (high << 4) | low;
        }
        Some(Self(bytes))
    }

    /// 转成小写十六进制（可直接存进数据库当幂等键）。
    #[must_use]
    pub fn to_hex(self) -> String {
        let mut text = String::with_capacity(64);
        for byte in self.0 {
            text.push(hex_digit(byte >> 4));
            text.push(hex_digit(byte & 0x0f));
        }
        text
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'a' + value - 10) as char,
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 一个待导入的候选文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// 文件路径（平台层给什么就是什么，本 crate 不解读）。
    pub path: String,
    /// 文件字节数。
    pub size: u64,
    /// 已算好的指纹。
    pub fingerprint: Fingerprint,
}

/// 被跳过的候选。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 同一批里已经出现过同一指纹。
    DuplicateInBatch,
    /// 书库里已经有同一指纹的书。
    AlreadyImported,
}

/// 一条跳过记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// 候选路径。
    pub path: String,
    /// 跳过原因。
    pub reason: SkipReason,
}

/// 被拒绝的候选（连登记都不该做）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// 路径为空。
    EmptyPath,
    /// 扩展名不是支持的格式。
    UnknownFormat,
    /// 文件名里没有可用的标题（例如只剩扩展名）。
    NoTitle,
}

/// 一条拒绝记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    /// 候选路径。
    pub path: String,
    /// 拒绝原因。
    pub reason: RejectReason,
}

/// 阶段①的登记计划。平台层照着它落盘与插库即可。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportPlan {
    /// 需要登记的书（按输入顺序）。
    pub to_register: Vec<Book>,
    /// 被跳过的候选。
    pub skipped: Vec<Skipped>,
    /// 被拒绝的候选。
    pub rejected: Vec<Rejected>,
}

impl ImportPlan {
    /// 计划登记的数量。
    #[must_use]
    pub fn register_count(&self) -> usize {
        self.to_register.len()
    }

    /// 是否有任何动作。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.to_register.is_empty() && self.skipped.is_empty() && self.rejected.is_empty()
    }
}

/// 生成登记计划。
///
/// `already_imported` 是书库里已有的指纹集合（由平台层从数据库查出来传进来）。
///
/// 规则（按优先级）：
/// 1. 路径为空 / 扩展名不认识 / 取不出标题 → 拒绝
/// 2. 指纹已在书库里 → 跳过（`AlreadyImported`）
/// 3. 指纹在本批里重复 → 跳过（`DuplicateInBatch`）
/// 4. 其余 → 登记
#[must_use]
pub fn plan_import(
    candidates: &[Candidate],
    already_imported: &HashSet<Fingerprint>,
) -> ImportPlan {
    let mut plan = ImportPlan::default();
    let mut seen_in_batch: HashSet<Fingerprint> = HashSet::new();

    for candidate in candidates {
        if candidate.path.trim().is_empty() {
            plan.rejected.push(Rejected {
                path: candidate.path.clone(),
                reason: RejectReason::EmptyPath,
            });
            continue;
        }

        // 复用 perch-model 的扩展名判断，保证与 Locator 层用同一套规则
        let Some(format) = Format::from_href(&candidate.path) else {
            plan.rejected.push(Rejected {
                path: candidate.path.clone(),
                reason: RejectReason::UnknownFormat,
            });
            continue;
        };

        let Some(title) = title_from_path(&candidate.path) else {
            plan.rejected.push(Rejected {
                path: candidate.path.clone(),
                reason: RejectReason::NoTitle,
            });
            continue;
        };

        if already_imported.contains(&candidate.fingerprint) {
            plan.skipped.push(Skipped {
                path: candidate.path.clone(),
                reason: SkipReason::AlreadyImported,
            });
            continue;
        }

        if !seen_in_batch.insert(candidate.fingerprint) {
            plan.skipped.push(Skipped {
                path: candidate.path.clone(),
                reason: SkipReason::DuplicateInBatch,
            });
            continue;
        }

        // 阶段①只登记，不解析内容
        let mut book = Book::register(
            title,
            format,
            candidate.size,
            candidate.fingerprint.to_hex(),
        );
        book.id = BookId::new();
        plan.to_register.push(book);
    }

    plan
}

/// 从路径里取书名：去掉目录与扩展名。
///
/// 空字符串或只剩扩展名时返回 `None`（这种候选没法在书架上显示）。
#[must_use]
pub fn title_from_path(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 同时兼容 `/` 与 `\\`（虽然移动端不会有 `\\`，但传入路径来自外部）
    let file_name = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed).trim();
    let stem = match file_name.rfind('.') {
        // `.epub` 这种只有扩展名的，stem 为空
        Some(index) if index > 0 => &file_name[..index],
        Some(_) => "",
        None => file_name,
    };
    if stem.trim().is_empty() {
        None
    } else {
        Some(stem.trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use perch_model::ImportState;

    fn candidate(path: &str, size: u64, seed: u8) -> Candidate {
        let head = vec![seed; 16];
        let tail = vec![seed.wrapping_add(1); 16];
        Candidate {
            path: path.to_owned(),
            size,
            fingerprint: Fingerprint::compute(size, &head, &tail),
        }
    }

    // ---------------- 指纹 ----------------

    #[test]
    fn fingerprint_is_deterministic() {
        let first = Fingerprint::compute(100, b"head", b"tail");
        let second = Fingerprint::compute(100, b"head", b"tail");
        assert_eq!(first, second);
    }

    #[test]
    fn fingerprint_changes_with_size_head_or_tail() {
        let base = Fingerprint::compute(100, b"head", b"tail");
        assert_ne!(base, Fingerprint::compute(101, b"head", b"tail"));
        assert_ne!(base, Fingerprint::compute(100, b"HEAD", b"tail"));
        assert_ne!(base, Fingerprint::compute(100, b"head", b"TAIL"));
    }

    #[test]
    fn fingerprint_distinguishes_segment_boundaries() {
        // 没有长度分隔时 ("ab","c") 与 ("a","bc") 会撞车
        let first = Fingerprint::compute(10, b"ab", b"c");
        let second = Fingerprint::compute(10, b"a", b"bc");
        assert_ne!(first, second);
    }

    #[test]
    fn fingerprint_hex_round_trips() {
        let original = Fingerprint::compute(42, b"hello", b"world");
        let hex = original.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(Fingerprint::from_hex(&hex).unwrap(), original);
        // 大写也接受
        assert_eq!(
            Fingerprint::from_hex(&hex.to_uppercase()).unwrap(),
            original
        );
        assert_eq!(original.to_string(), hex);
    }

    #[test]
    fn fingerprint_hex_rejects_garbage() {
        assert!(Fingerprint::from_hex("").is_none());
        assert!(Fingerprint::from_hex("abcd").is_none());
        assert!(Fingerprint::from_hex(&"z".repeat(64)).is_none());
    }

    #[test]
    fn small_files_with_overlapping_samples_are_fine() {
        // 小于 128KB 的文件首尾会重叠，仍然只是一个"多算了点"的输入
        let bytes = b"tiny";
        let fingerprint = Fingerprint::compute(bytes.len() as u64, bytes, bytes);
        assert_eq!(fingerprint, Fingerprint::compute(4, b"tiny", b"tiny"));
    }

    // ---------------- 标题提取 ----------------

    #[test]
    fn title_comes_from_file_name() {
        assert_eq!(title_from_path("/books/三体.epub").as_deref(), Some("三体"));
        assert_eq!(title_from_path("三体.pdf").as_deref(), Some("三体"));
        assert_eq!(
            title_from_path(r"C:\books\Dune.txt").as_deref(),
            Some("Dune")
        );
        // 没有扩展名也接受
        assert_eq!(title_from_path("/books/README").as_deref(), Some("README"));
        // 名字里带点：只去掉最后一段扩展名
        assert_eq!(
            title_from_path("/b/chapter.1.final.epub").as_deref(),
            Some("chapter.1.final")
        );
    }

    #[test]
    fn title_rejects_degenerate_names() {
        assert!(title_from_path("").is_none());
        assert!(title_from_path("   ").is_none());
        assert!(title_from_path("/books/.epub").is_none());
        assert!(title_from_path("/books/").is_none());
    }

    // ---------------- 登记计划 ----------------

    #[test]
    fn plans_a_single_file() {
        let plan = plan_import(&[candidate("/b/三体.epub", 1024, 1)], &HashSet::new());
        assert_eq!(plan.register_count(), 1);
        assert!(plan.skipped.is_empty());
        assert!(plan.rejected.is_empty());

        let book = &plan.to_register[0];
        assert_eq!(book.title, "三体");
        assert_eq!(book.format, Format::Epub);
        assert_eq!(book.size_bytes, 1024);
        assert_eq!(book.state, ImportState::Registered, "阶段①只到 registered");
        book.validate().unwrap();
    }

    #[test]
    fn duplicate_within_batch_is_skipped() {
        let first = candidate("/b/三体.epub", 1024, 1);
        let same_content = Candidate {
            path: "/b/三体副本.epub".to_owned(),
            ..first.clone()
        };
        let plan = plan_import(&[first, same_content], &HashSet::new());
        assert_eq!(plan.register_count(), 1);
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].reason, SkipReason::DuplicateInBatch);
        assert_eq!(plan.skipped[0].path, "/b/三体副本.epub");
    }

    #[test]
    fn already_imported_fingerprint_is_skipped() {
        let existing = candidate("/b/三体.epub", 1024, 1);
        let mut imported = HashSet::new();
        imported.insert(existing.fingerprint);

        let plan = plan_import(&[existing], &imported);
        assert!(plan.to_register.is_empty());
        assert_eq!(plan.skipped[0].reason, SkipReason::AlreadyImported);
    }

    #[test]
    fn unknown_extension_is_rejected() {
        let plan = plan_import(&[candidate("/b/cover.jpg", 100, 2)], &HashSet::new());
        assert!(plan.to_register.is_empty());
        assert_eq!(plan.rejected[0].reason, RejectReason::UnknownFormat);
    }

    #[test]
    fn empty_path_is_rejected() {
        let plan = plan_import(&[candidate("  ", 100, 3)], &HashSet::new());
        assert_eq!(plan.rejected[0].reason, RejectReason::EmptyPath);
    }

    #[test]
    fn degenerate_file_name_is_rejected() {
        let plan = plan_import(&[candidate("/b/.epub", 100, 4)], &HashSet::new());
        assert_eq!(plan.rejected[0].reason, RejectReason::NoTitle);
    }

    #[test]
    fn plan_preserves_input_order() {
        let candidates = vec![
            candidate("/b/甲.epub", 100, 11),
            candidate("/b/乙.pdf", 200, 12),
            candidate("/b/丙.txt", 300, 13),
        ];
        let plan = plan_import(&candidates, &HashSet::new());
        let titles: Vec<&str> = plan
            .to_register
            .iter()
            .map(|book| book.title.as_str())
            .collect();
        assert_eq!(titles, vec!["甲", "乙", "丙"]);
        assert_eq!(plan.to_register[1].format, Format::Pdf);
        assert_eq!(plan.to_register[2].format, Format::Txt);
    }

    #[test]
    fn mixed_batch_produces_all_three_outcomes() {
        let good = candidate("/b/好的.epub", 100, 21);
        let duplicate = Candidate {
            path: "/b/重复.epub".to_owned(),
            ..good.clone()
        };
        let bad = candidate("/b/坏.jpg", 50, 22);

        let plan = plan_import(&[good, duplicate, bad], &HashSet::new());
        assert_eq!(plan.register_count(), 1);
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.rejected.len(), 1);
        assert!(!plan.is_empty());
    }

    #[test]
    fn empty_input_gives_empty_plan() {
        let plan = plan_import(&[], &HashSet::new());
        assert!(plan.is_empty());
    }

    // ---------------- 性能基准（手动运行） ----------------

    /// 1000 本书的**阶段①**耗时基准。
    ///
    /// 默认被 `#[ignore]` 跳过：计时断言在 CI 上会抖。需要时手动跑：
    ///
    /// ```text
    /// cargo test -p perch-import -- --ignored --nocapture
    /// ```
    ///
    /// 这里只测 **CPU 部分**（算指纹 + 规划）。真实耗时还要加上文件 I/O 与落盘，
    /// 所以这是"1000 本 <10s"的**下界**，不是完整结论。
    #[test]
    #[ignore = "性能基准，手动运行：cargo test -p perch-import -- --ignored --nocapture"]
    fn bench_register_one_thousand_books() {
        let books = 1000usize;
        // 每本按 2MB 估，采样首尾各 64KB
        let sample = vec![0xABu8; SAMPLE_BYTES];
        let expected_size = 2 * 1024 * 1024u64;

        let started = std::time::Instant::now();
        let candidates: Vec<Candidate> = (0..books)
            .map(|index| {
                // 让每本的采样内容不同，避免指纹全部相同
                let mut head = sample.clone();
                head[0] = (index % 256) as u8;
                head[1] = ((index / 256) % 256) as u8;
                Candidate {
                    path: format!("/library/book-{index}.epub"),
                    size: expected_size,
                    fingerprint: Fingerprint::compute(expected_size, &head, &sample),
                }
            })
            .collect();
        let hashing = started.elapsed();

        let planning_started = std::time::Instant::now();
        let plan = plan_import(&candidates, &HashSet::new());
        let planning = planning_started.elapsed();

        let total = started.elapsed();
        println!(
            "阶段①基准：{books} 本 | 指纹 {} ms | 规划 {} ms | 合计 {} ms",
            hashing.as_millis(),
            planning.as_millis(),
            total.as_millis()
        );

        assert_eq!(plan.register_count(), books, "1000 本都应登记");
        // 留出充裕余量：这里只测 CPU，真实瓶颈在 I/O
        assert!(
            total.as_secs_f64() < 10.0,
            "阶段① CPU 部分应在 10 秒内，实际 {} ms",
            total.as_millis()
        );
    }
}
