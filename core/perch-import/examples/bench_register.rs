//! 阶段①登记吞吐基准：**"1000 本 < 10s 上架"这个头号目标到底能不能达成**。
//!
//! 阶段①的设计红线是「**不打开文件内容**」——只读首尾各 64KB 算指纹，
//! 然后纯内存地做去重与登记规划。所以真正的成本是：
//!
//! 1. 1000 次 `open` + 2 次 `read`（首、尾各 64KB）+ `close` 的系统调用
//! 2. 1000 次 SHA-256（128KB/本）
//! 3. 一次 `plan_import`（纯内存）
//!
//! 本基准把这三段分开计时。用**稀疏文件**造 1000 本 2MB 的书：
//! 逻辑大小真实（`stat` 返回 2MB），但不占磁盘。这样测的是
//! 读取与哈希的固定开销，而不是"造 2GB 数据"的时间。
//!
//! ⚠️ 这不是 `docs/10` §五 里那个被取消的「CJK 索引体积/速度实测」。
//! 那个是阶段③的索引测量，已明确不做；这里是阶段①的登记吞吐，
//! 是 `docs/01` M1 的验收标准。
//!
//! 用法：
//!
//! ```bash
//! cargo run --release -p perch-import --example bench_register
//! cargo run --release -p perch-import --example bench_register -- 2000 8192
//! ```
//!
//! 参数：`[本数] [每本体量字节数]`，默认 1000 与 2 MiB。

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::time::Instant;

use perch_import::{Candidate, Fingerprint, SAMPLE_BYTES, plan_import};

/// 阶段①的时间预算（`docs/01` M1 验收标准）。
const BUDGET_MS: f64 = 10_000.0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let count: usize = match args.next() {
        Some(text) => text.parse()?,
        None => 1000,
    };
    let size: u64 = match args.next() {
        Some(text) => text.parse()?,
        None => 2 * 1024 * 1024,
    };

    let dir = std::env::temp_dir().join(format!("perch-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;

    // 造书：稀疏文件（逻辑大小真实、不占磁盘）+ 首尾写入唯一标记。
    //
    // ⚠️ 唯一标记是必须的：稀疏文件读出来全是零，若不加标记，
    // 1000 个文件的指纹会完全相同，`plan_import` 会把 999 本当重复跳过——
    // 那样测到的是"1 次登记 + 999 次查表"，而不是 1000 本真登记。
    let mut paths = Vec::with_capacity(count);
    for index in 0..count {
        let path = dir.join(format!("book-{index:05}.epub"));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(&path)?;
        file.set_len(size)?;
        write_marker(&mut file, 0, index)?;
        if size >= 16 {
            write_marker(&mut file, size - 16, index)?;
        }
        paths.push(path);
    }
    println!("=== 阶段①登记吞吐 ===\n");
    println!(
        "本数: {count}，每本逻辑大小: {} KiB（稀疏文件）",
        size / 1024
    );

    // ---- 1) 读首尾 + 算指纹 ----
    let start = Instant::now();
    let mut candidates = Vec::with_capacity(count);
    let mut head = vec![0u8; SAMPLE_BYTES];
    let mut tail = vec![0u8; SAMPLE_BYTES];
    for path in &paths {
        let mut file = OpenOptions::new().read(true).open(path)?;
        let actual = file.metadata()?.len();

        let head_len = read_head(&mut file, &mut head)?;
        let tail_len = read_tail(&mut file, actual, &mut tail)?;

        let fingerprint = Fingerprint::compute(actual, &head[..head_len], &tail[..tail_len]);
        candidates.push(Candidate {
            path: path.to_string_lossy().into_owned(),
            size: actual,
            fingerprint,
        });
    }
    let fingerprint_ms = elapsed_ms(start);

    // ---- 2) 去重与登记规划（纯内存）----
    let start = Instant::now();
    let plan = plan_import(&candidates, &HashSet::new());
    let plan_ms = elapsed_ms(start);

    let total_ms = fingerprint_ms + plan_ms;
    println!("\n--- 分段耗时 ---");
    println!(
        "读首尾 + 指纹: {fingerprint_ms:8.1} ms  ({:.3} ms/本)",
        fingerprint_ms / count as f64
    );
    println!("规划（纯内存）: {plan_ms:8.1} ms");
    println!("合计:           {total_ms:8.1} ms");
    println!("\n--- 结果 ---");
    println!(
        "登记 {} 本，跳过 {}，拒绝 {}",
        plan.register_count(),
        plan.skipped.len(),
        plan.rejected.len()
    );
    println!("吞吐: {:.0} 本/秒", count as f64 / (total_ms / 1000.0));

    println!("\n--- 本基准**不含**下列成本（它们同样属于阶段①）---");
    println!("  1. 把文件复制/硬链接进沙盒（iOS 侧 NSFileCoordinator，可能不便宜）");
    println!("  2. 冷页缓存 / iCloud 下载");
    println!("  3. SQLite 写 1000 行");
    println!("  所以这里的数字是**下界**：只证明「读首尾+哈希+规划」这一步远不是瓶颈，");
    println!("  真正的风险在沙盒复制，需要真机才能量。");

    let passed = total_ms < BUDGET_MS;
    println!(
        "\n预算 {BUDGET_MS:.0} ms（docs/01 M1）：{}（用掉 {:.1}%）",
        if passed { "✓ 通过" } else { "✗ 超出" },
        total_ms / BUDGET_MS * 100.0
    );

    std::fs::remove_dir_all(&dir)?;
    if !passed {
        std::process::exit(1);
    }
    Ok(())
}

/// 在指定偏移写入 16 字节唯一标记（含序号与固定前缀）。
fn write_marker(file: &mut File, offset: u64, index: usize) -> Result<(), std::io::Error> {
    use std::io::Write;
    let mut marker = [0u8; 16];
    marker[..8].copy_from_slice(b"PERCHBNC");
    marker[8..].copy_from_slice(&(index as u64).to_le_bytes());
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&marker)?;
    file.flush()?;
    Ok(())
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn read_head(file: &mut File, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
    file.seek(SeekFrom::Start(0))?;
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

fn read_tail(file: &mut File, size: u64, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
    let want = u64::try_from(buffer.len()).unwrap_or(u64::MAX).min(size);
    let start = size - want;
    file.seek(SeekFrom::Start(start))?;
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}
