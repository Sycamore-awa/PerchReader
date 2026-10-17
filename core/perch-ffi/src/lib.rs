//! FFI 边界：**单一入口** + **JSON 契约**。
//!
//! # 为什么是单一入口
//!
//! 只暴露两个符号：
//!
//! ```text
//! perch_call(method: utf8, args: json) -> json   // 返回堆字符串指针，调用方负责释放
//! perch_string_free(ptr)
//! ```
//!
//! 好处：新增能力不需要改 Dart 侧的绑定代码，只要在 [`dispatch`] 里加一条分支；
//! 也不会因为函数签名变化而让两边的 ABI 悄悄失配。
//!
//! # 为什么用 JSON 而不是 `flutter_rust_bridge`
//!
//! `flutter_rust_bridge` 的代码生成更优雅，但它**只能在没有 Mac 的情况下写、无法验证**。
//! 而 JSON 契约可以用 `dart:ffi` 在本机直接加载 `.so` 调用 —— 整条边界都能在 Linux 上跑通。
//!
//! 契约（method 名 + JSON 结构）是稳定层，将来换成 FRB 只是换传输层，上层不用改。
//!
//! # 约定
//!
//! - 入参永远是 UTF-8 JSON（无参数时传 `{}`）
//! - 出参永远是 UTF-8 JSON；成功是业务对象，失败是 `{"error":{"code":..,"message":..}}`
//! - **不 panic**：任何错误都转成 `error` 对象。这是"崩溃率 < 0.1%"目标的边界防线
//! - 字节数组用十六进制字符串传递，避免 base64 依赖（也便于日志里看）
//!
//! # ⚠️ unsafe 的唯一集中地
//!
//! 本 crate 是工作区里**唯一**不继承 `forbid(unsafe_code)` 的 crate —— 跨语言边界必须
//! 处理裸指针。所有 `unsafe` 都集中在文件末尾的两个导出函数里，业务逻辑一律 safe。

#![allow(unsafe_code)]

use std::ffi::CString;

use perch_import::Fingerprint;
use perch_model::{Locator, ModelError};
use perch_pagecurl::{Corner, CurlInput, Rect, Vec2, curl};
use serde_json::{Value, json};

/// 业务版本号，供 Dart 侧做兼容性断言。
const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 同时最多允许打开多少本书的索引。
///
/// 手机上不能无限开：每个索引都持有整本正文（[`perch_jobs::BookText`]）+
/// 倒排表。取 8 是为了"书架 + 阅读器 + 搜索面板"这类场景够用，
/// 同时把最坏内存占用兜住。
const MAX_OPEN_BOOKS: usize = 8;

/// 一次搜索默认最多返回多少条。
///
/// 搜索面板一次也展示不了更多；上限小一点也能让大书上的单次调用更快。
const DEFAULT_SEARCH_LIMIT: usize = 50;

/// 一本书的已打开状态：正文 + 章节目录 + 倒排索引。
struct OpenBook {
    text: perch_jobs::BookText,
    index: perch_index::Index,
}

/// 有状态对象的句柄表。
///
/// Dart 侧拿到的是 `u64` 句柄；真正的 Rust 对象留在这里。
/// **句柄自增且永不复用**——这样"关掉之后又拿旧句柄调用"会得到
/// `BOOK_BAD_HANDLE`，而不是误操作到另一个对象上。
#[derive(Default)]
struct Registry {
    next: u64,
    open: std::collections::HashMap<u64, OpenBook>,
}

static REGISTRY: std::sync::OnceLock<std::sync::Mutex<Registry>> = std::sync::OnceLock::new();

/// 借用句柄表。
///
/// 互斥锁中毒时用 `into_inner()` 继续——`Registry` 里没有会被破坏的不变量，
/// 为了"一次 panic"就让后续所有调用永久失败不值得。
fn registry() -> std::sync::MutexGuard<'static, Registry> {
    let mutex = REGISTRY.get_or_init(|| std::sync::Mutex::new(Registry::default()));
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Registry {
    fn insert(&mut self, book: OpenBook) -> Result<u64, FfiError> {
        if self.open.len() >= MAX_OPEN_BOOKS {
            return Err(FfiError::new(
                "BOOK_TOO_MANY_OPEN",
                format!("同时最多打开 {MAX_OPEN_BOOKS} 本书，请先 close"),
            ));
        }
        let handle = self.next;
        self.next = self.next.saturating_add(1);
        self.open.insert(handle, book);
        Ok(handle)
    }

    fn get(&self, handle: u64) -> Result<&OpenBook, FfiError> {
        self.open.get(&handle).ok_or_else(|| {
            FfiError::new(
                "BOOK_BAD_HANDLE",
                format!("句柄 {handle} 不存在（可能已 close 或从未 open）"),
            )
        })
    }
}

/// 统一错误对象。
#[derive(Debug)]
struct FfiError {
    code: &'static str,
    message: String,
}

impl FfiError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn bad_args(message: impl Into<String>) -> Self {
        Self::new("FFI_BAD_ARGS", message)
    }

    fn from_model(error: &ModelError) -> Self {
        Self::new(error.code(), error.to_string())
    }

    fn from_sync(error: &perch_sync::SyncError) -> Self {
        Self::new(error.code(), error.to_string())
    }

    fn to_value(&self) -> Value {
        json!({ "error": { "code": self.code, "message": self.message } })
    }
}

/// 主分发：方法名 + JSON 入参 → JSON 出参。
pub fn dispatch(method: &str, args: &str) -> Value {
    match call(method, args) {
        Ok(value) => value,
        Err(error) => error.to_value(),
    }
}

fn call(method: &str, args: &str) -> Result<Value, FfiError> {
    let args: Value = if args.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(args).map_err(|err| FfiError::bad_args(err.to_string()))?
    };

    match method {
        "version" => Ok(json!({ "core": CORE_VERSION })),

        // ---- 定位（perch-model）----
        "locator.validate" => {
            let locator: Locator =
                serde_json::from_value(args).map_err(|err| FfiError::bad_args(err.to_string()))?;
            locator
                .validate()
                .map_err(|err| FfiError::from_model(&err))?;
            Ok(json!({ "valid": true, "format": locator.format().map(|f| f.as_str()) }))
        }

        // ---- 导入（perch-import）----
        "fingerprint" => {
            let size = args
                .get("size")
                .and_then(Value::as_u64)
                .ok_or_else(|| FfiError::bad_args("缺少 size"))?;
            let head = hex_to_bytes(required_str(&args, "head_hex")?)?;
            let tail = hex_to_bytes(required_str(&args, "tail_hex")?)?;
            let fingerprint = Fingerprint::compute(size, &head, &tail);
            Ok(json!({ "fingerprint": fingerprint.to_hex() }))
        }

        // ---- 阶段②精修（perch-jobs）----
        "jobs.refine" => {
            let bytes = hex_to_bytes(required_str(&args, "bytes_hex")?)?;
            let max_chars = args
                .get("max_chars")
                .and_then(Value::as_u64)
                .unwrap_or(perch_jobs::ExtractOptions::default().max_chars);
            let refinement =
                perch_jobs::refine_epub(&bytes, perch_jobs::ExtractOptions { max_chars })
                    .map_err(|error| FfiError::new(error.code(), error.to_string()))?;
            serde_json::to_value(&refinement).map_err(|error| FfiError::bad_args(error.to_string()))
        }

        // 打开一本书：抽正文 + 建索引，返回句柄。后续搜索复用这个索引。
        "book.open" => {
            let bytes = hex_to_bytes(required_str(&args, "bytes_hex")?)?;
            let chunk_chars = args
                .get("chunk_chars")
                .and_then(Value::as_u64)
                .unwrap_or(perch_index::DEFAULT_CHUNK_CHARS);
            let text = perch_jobs::extract_epub_text(&bytes, perch_jobs::ExtractOptions::default())
                .map_err(|error| FfiError::new(error.code(), error.to_string()))?;
            let index = perch_jobs::index_book(&text, chunk_chars);
            let stats = index.stats();
            let char_count = text.char_count();
            let chapter_count = text.chapters.len();
            let truncated = text.truncated;
            let handle = registry().insert(OpenBook { text, index })?;
            Ok(json!({
                "handle": handle,
                "charCount": char_count,
                "chapterCount": chapter_count,
                "chunks": stats.chunks,
                "tokens": stats.tokens,
                "truncated": truncated,
            }))
        }

        // 在已打开的索引里检索；命中会还原成粗定位 Locator。
        "book.search" => {
            let handle = required_u64(&args, "handle")?;
            let query = required_str(&args, "query")?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map_or(DEFAULT_SEARCH_LIMIT, |n| n as usize);
            let guard = registry();
            let book = guard.get(handle)?;
            let hits: Vec<Value> = book
                .index
                .search(query)
                .into_iter()
                .take(limit)
                .map(|hit| {
                    let locator = perch_jobs::locate_hit(&book.text, &hit);
                    json!({
                        "charStart": hit.char_start,
                        "charEnd": hit.char_end,
                        "snippet": hit.snippet,
                        "locator": locator,
                    })
                })
                .collect();
            Ok(json!({ "hits": hits }))
        }

        // 释放一本书的索引。
        "book.close" => {
            let handle = required_u64(&args, "handle")?;
            let removed = registry().open.remove(&handle).is_some();
            Ok(json!({ "closed": removed }))
        }

        // ---- PDF（perch-pdf）----
        "pdf.analyze" => {
            let bytes = hex_to_bytes(required_str(&args, "bytes_hex")?)?;
            let info = perch_pdf::analyze(&bytes)
                .map_err(|error| FfiError::new(error.code(), error.to_string()))?;
            serde_json::to_value(&info).map_err(|error| FfiError::bad_args(error.to_string()))
        }

        // ---- TXT（perch-txt）----
        "txt.analyze" => {
            let bytes = hex_to_bytes(required_str(&args, "bytes_hex")?)?;
            let document = perch_txt::TxtDocument::from_bytes(&bytes);
            let meta = document.meta();
            Ok(json!({
                "encoding": meta.encoding,
                "certain_encoding": meta.certain_encoding,
                "chars": meta.chars,
                "lines": meta.lines,
                "had_decode_errors": meta.had_decode_errors,
                "text": document.text(),
            }))
        }

        // ---- 检索（perch-index）----
        "index.search" => {
            let text = required_str(&args, "text")?;
            let query = required_str(&args, "query")?;
            let chunk = args
                .get("chunk_chars")
                .and_then(Value::as_u64)
                .unwrap_or(perch_index::DEFAULT_CHUNK_CHARS);
            let index = perch_index::Index::build(text.to_owned(), chunk);
            let hits: Vec<Value> = index
                .search(query)
                .into_iter()
                .map(|hit| {
                    json!({
                        "char_start": hit.char_start,
                        "char_end": hit.char_end,
                        "chunk": hit.chunk,
                        "snippet": hit.snippet,
                    })
                })
                .collect();
            Ok(json!({ "hits": hits }))
        }

        // ---- 卷页几何（perch-pagecurl）----
        "curl" => {
            let page = Rect::new(
                number(&args, "page_w")? as f32,
                number(&args, "page_h")? as f32,
            );
            let corner = match required_str(&args, "corner")? {
                "top_left" => Corner::TopLeft,
                "top_right" => Corner::TopRight,
                "bottom_left" => Corner::BottomLeft,
                "bottom_right" => Corner::BottomRight,
                other => return Err(FfiError::bad_args(format!("未知 corner: {other}"))),
            };
            let input = CurlInput {
                page,
                corner,
                drag: Vec2::new(
                    number(&args, "drag_x")? as f32,
                    number(&args, "drag_y")? as f32,
                ),
                radius: number(&args, "radius")? as f32,
            };
            match curl(&input) {
                None => Ok(json!({ "geometry": Value::Null })),
                Some(geometry) => Ok(json!({
                    "geometry": {
                        "fold_start": point(geometry.fold_start),
                        "fold_end": point(geometry.fold_end),
                        "midpoint": point(geometry.midpoint),
                        "direction": point(geometry.direction),
                        "radius": geometry.radius,
                        "curl_angle": geometry.curl_angle,
                        "band_extent": geometry.band_extent,
                        "progress": geometry.progress,
                        "front_outline": geometry.front_outline.iter().map(|p| point(*p)).collect::<Vec<_>>(),
                        "shadow": {
                            "start": point(geometry.shadow.start),
                            "end": point(geometry.shadow.end),
                            "start_opacity": geometry.shadow.start_opacity,
                            "end_opacity": geometry.shadow.end_opacity,
                        }
                    }
                })),
            }
        }

        // ---- 同步（perch-sync）----
        "hlc.parse" => {
            let text = required_str(&args, "hlc")?;
            match perch_sync::Hlc::parse(text) {
                None => Err(FfiError::new("SYNC_BAD_HLC", "HLC 格式非法")),
                Some(hlc) => Ok(json!({
                    "wall_ms": hlc.wall_ms(),
                    "counter": hlc.counter(),
                    "device": hlc.device(),
                })),
            }
        }

        // 一次往返完成双向同步的合并：本地状态 + 远端变更 → 新状态 + 冲突统计
        "sync.merge" => {
            let local = changes_arg(&args, "local")?;
            let remote = changes_arg(&args, "remote")?;
            let mut log = perch_sync::ChangeLog::from_changes(&local)
                .map_err(|err| FfiError::from_sync(&err))?;
            let report = log
                .apply_all(&remote)
                .map_err(|err| FfiError::from_sync(&err))?;
            Ok(json!({
                "state": log.to_changes(),
                "stats": report.stats,
                "conflictRate": report.conflict_rate(),
                "effects": report.effects.iter().map(|e| e.as_str()).collect::<Vec<_>>(),
            }))
        }

        // 增量同步的服务端实现：给游标 + 页大小，返回一页变更
        "sync.delta" => {
            let state = changes_arg(&args, "state")?;
            let log = perch_sync::ChangeLog::from_changes(&state)
                .map_err(|err| FfiError::from_sync(&err))?;
            let since = match args.get("since").and_then(Value::as_str) {
                None => None,
                Some(text) => Some(
                    perch_sync::Hlc::parse(text)
                        .ok_or_else(|| FfiError::new("SYNC_BAD_HLC", "since 的 HLC 格式非法"))?,
                ),
            };
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map_or(perch_sync::DEFAULT_PAGE_LIMIT, |n| n as usize);
            let page = log.changes_since(since.as_ref(), limit);
            Ok(json!({
                "changes": page.changes,
                "nextCursor": page.next_cursor.map(|hlc| hlc.to_string()),
                "hasMore": page.has_more,
            }))
        }

        other => Err(FfiError::new(
            "FFI_UNKNOWN_METHOD",
            format!("未知方法: {other}"),
        )),
    }
}

/// 取一个必填的无符号整数字段。
fn required_u64(args: &Value, field: &str) -> Result<u64, FfiError> {
    args.get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| FfiError::bad_args(format!("缺少或非法的 {field}")))
}

/// 从入参里取一个变更数组（字段缺省视为空数组）。
fn changes_arg(args: &Value, field: &str) -> Result<Vec<perch_sync::Change>, FfiError> {
    match args.get(field) {
        None => Ok(Vec::new()),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|error| FfiError::bad_args(format!("{field}: {error}"))),
    }
}

fn point(value: Vec2) -> Value {
    json!({ "x": value.x, "y": value.y })
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, FfiError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| FfiError::bad_args(format!("缺少字符串字段 {key}")))
}

fn number(args: &Value, key: &str) -> Result<f64, FfiError> {
    args.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| FfiError::bad_args(format!("缺少数字字段 {key}")))
}

/// 十六进制字符串 → 字节。
fn hex_to_bytes(text: &str) -> Result<Vec<u8>, FfiError> {
    // 不用 is_multiple_of：它 1.87 才稳定，而工作区 MSRV 是 1.85
    if text.len() % 2 != 0 {
        return Err(FfiError::bad_args("十六进制长度必须是偶数"));
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let high = hex_value(chunk[0])?;
        let low = hex_value(chunk[1])?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn hex_value(byte: u8) -> Result<u8, FfiError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(FfiError::bad_args("十六进制里有非法字符")),
    }
}

// ---------------------------------------------------------------------------
// 以下为唯一的 unsafe 区域：跨越 C ABI 的裸指针
// ---------------------------------------------------------------------------

/// 调用一个方法。
///
/// # Safety
///
/// `method_ptr` / `args_ptr` 必须是调用方保证在本次调用期间有效的 UTF-8 内存，
/// 长度分别是 `method_len` / `args_len` 字节。返回的指针必须交给
/// [`perch_string_free`] 释放，且只能释放一次。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perch_call(
    method_ptr: *const u8,
    method_len: usize,
    args_ptr: *const u8,
    args_len: usize,
) -> *mut std::ffi::c_char {
    // 任何输入问题都不能 panic：panic 跨过 FFI 边界是未定义行为
    let method = match slice_to_str(method_ptr, method_len) {
        Some(text) => text,
        None => {
            return to_c_string(
                &json!({ "error": { "code": "FFI_BAD_METHOD_PTR", "message": "方法名不是合法 UTF-8" } }),
            );
        }
    };
    let args = match slice_to_str(args_ptr, args_len) {
        Some(text) => text,
        None => {
            return to_c_string(
                &json!({ "error": { "code": "FFI_BAD_ARGS_PTR", "message": "参数不是合法 UTF-8" } }),
            );
        }
    };
    to_c_string(&dispatch(method, args))
}

/// 释放 [`perch_call`] 返回的字符串。
///
/// # Safety
///
/// `ptr` 必须来自 [`perch_call`]，且只能释放一次；传空指针是安全的（直接返回）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perch_string_free(ptr: *mut std::ffi::c_char) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: 指针由 CString::into_raw 产生，调用方承诺只释放一次
    drop(unsafe { CString::from_raw(ptr) });
}

/// 把裸指针 + 长度转成 `&str`（非法 UTF-8 或空指针返回 `None`）。
fn slice_to_str(ptr: *const u8, len: usize) -> Option<&'static str> {
    if ptr.is_null() || len == 0 {
        return Some("");
    }
    // SAFETY: 调用方保证 [ptr, ptr+len) 在本次调用期间有效
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(slice).ok()
}

fn to_c_string(value: &Value) -> *mut std::ffi::c_char {
    let text = value.to_string();
    // 兜底：万一 JSON 里出现内嵌 NUL（实际不会），退回一个固定错误
    match CString::new(text) {
        Ok(string) => string.into_raw(),
        Err(_) => CString::new(r#"{"error":{"code":"FFI_NUL","message":"输出含 NUL"}}"#)
            .map(CString::into_raw)
            .unwrap_or(std::ptr::null_mut()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn call_ok(method: &str, args: &str) -> Value {
        let value = dispatch(method, args);
        assert!(value.get("error").is_none(), "{method} 出错: {value}");
        value
    }

    #[test]
    fn version_reports_core_version() {
        let value = call_ok("version", "{}");
        assert_eq!(value["core"], CORE_VERSION);
    }

    #[test]
    fn unknown_method_returns_error_object() {
        let value = dispatch("nope", "{}");
        assert_eq!(value["error"]["code"], "FFI_UNKNOWN_METHOD");
    }

    #[test]
    fn locator_validate_accepts_good_and_rejects_bad() {
        let good = json!({
            "href": "/c1.xhtml",
            "locations": { "progression": 0.5 }
        });
        let value = call_ok("locator.validate", &good.to_string());
        assert_eq!(value["valid"], true);

        let bad = json!({ "href": "", "locations": {} });
        let value = dispatch("locator.validate", &bad.to_string());
        assert_eq!(value["error"]["code"], "MODEL_EMPTY_FIELD");
    }

    #[test]
    fn fingerprint_is_stable_across_calls() {
        let args = json!({ "size": 4, "head_hex": "6162", "tail_hex": "6364" });
        let first = call_ok("fingerprint", &args.to_string());
        let second = call_ok("fingerprint", &args.to_string());
        assert_eq!(first["fingerprint"], second["fingerprint"]);
        assert_eq!(first["fingerprint"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn txt_analyze_detects_utf8_and_counts() {
        let args = json!({ "bytes_hex": "e4b8ade69687" }); // "中文"
        let value = call_ok("txt.analyze", &args.to_string());
        assert_eq!(value["text"], "中文");
        assert_eq!(value["chars"], 2);
        assert_eq!(value["encoding"], "UTF-8");
        assert_eq!(value["certain_encoding"], true);
    }

    #[test]
    fn txt_analyze_detects_gbk() {
        let args = json!({ "bytes_hex": "d6d0cec4" }); // "中文" 的 GBK
        let value = call_ok("txt.analyze", &args.to_string());
        assert_eq!(value["text"], "中文");
        assert_eq!(value["certain_encoding"], false, "猜出来的编码不应标为确定");
    }

    #[test]
    fn index_search_returns_hits_with_offsets() {
        let args = json!({ "text": "前面的内容。中文测试。", "query": "中文测试" });
        let value = call_ok("index.search", &args.to_string());
        let hits = value["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["char_start"], 6);
        assert_eq!(hits[0]["char_end"], 10);
    }

    #[test]
    fn curl_returns_null_geometry_when_not_dragged() {
        let args = json!({
            "page_w": 400, "page_h": 700, "corner": "bottom_right",
            "drag_x": 400, "drag_y": 700, "radius": 20
        });
        let value = call_ok("curl", &args.to_string());
        assert!(value["geometry"].is_null());
    }

    #[test]
    fn curl_returns_geometry_when_dragged() {
        let args = json!({
            "page_w": 400, "page_h": 700, "corner": "bottom_right",
            "drag_x": 200, "drag_y": 400, "radius": 24
        });
        let value = call_ok("curl", &args.to_string());
        let geometry = &value["geometry"];
        assert!(geometry["curl_angle"].as_f64().unwrap() > 0.0);
        assert!(geometry["front_outline"].as_array().unwrap().len() >= 3);
    }

    #[test]
    fn hlc_parse_round_trips() {
        let value = call_ok("hlc.parse", r#"{"hlc":"100-2-phoneA"}"#);
        assert_eq!(value["wall_ms"], 100);
        assert_eq!(value["counter"], 2);
        assert_eq!(value["device"], "phoneA");

        let bad = dispatch("hlc.parse", r#"{"hlc":"nope"}"#);
        assert_eq!(bad["error"]["code"], "SYNC_BAD_HLC");
    }

    #[test]
    fn bad_json_args_produce_error_not_panic() {
        let value = dispatch("fingerprint", "{not json");
        assert_eq!(value["error"]["code"], "FFI_BAD_ARGS");
    }

    #[test]
    fn missing_fields_produce_error_not_panic() {
        let value = dispatch("fingerprint", "{}");
        assert_eq!(value["error"]["code"], "FFI_BAD_ARGS");
    }

    #[test]
    fn c_abi_round_trip_through_raw_pointers() {
        // 直接走一遍导出函数的路径（含 free），确保指针处理正确
        let method = "version";
        let args = "{}";
        let result =
            unsafe { perch_call(method.as_ptr(), method.len(), args.as_ptr(), args.len()) };
        assert!(!result.is_null());
        let text = unsafe { std::ffi::CStr::from_ptr(result) }
            .to_str()
            .unwrap()
            .to_owned();
        unsafe { perch_string_free(result) };

        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["core"], CORE_VERSION);
    }

    #[test]
    fn null_pointer_free_is_safe() {
        unsafe { perch_string_free(std::ptr::null_mut()) };
    }

    // ---------- 同步（perch-sync 的传输层）----------

    const BOOK: &str = "11111111-1111-4111-8111-111111111111";

    /// 一条进度变更的 JSON（`hlc` 是文本形式）。
    fn progress_change(hlc: &str, progression: f64) -> Value {
        json!({
            "book": BOOK,
            "hlc": hlc,
            "op": "set",
            "value": {
                "kind": "progress",
                "locator": { "href": "c1.xhtml", "locations": { "progression": progression } }
            }
        })
    }

    fn merge(local: Vec<Value>, remote: Vec<Value>) -> Value {
        let args = json!({ "local": local, "remote": remote }).to_string();
        dispatch("sync.merge", &args)
    }

    #[test]
    fn sync_merge_initializes_from_empty() {
        let out = merge(vec![], vec![progress_change("100-0-iphone", 0.5)]);
        assert_eq!(out["state"].as_array().unwrap().len(), 1);
        assert_eq!(out["effects"][0], "initialized");
        assert_eq!(out["stats"]["initialized"], 1);
        assert_eq!(out["conflictRate"], 0.0);
    }

    #[test]
    fn sync_merge_ignores_older_remote() {
        let out = merge(
            vec![progress_change("200-0-iphone", 0.9)],
            vec![progress_change("100-0-ipad", 0.1)],
        );
        assert_eq!(out["effects"][0], "staleIgnored");
        assert_eq!(out["stats"]["staleIgnored"], 1);
        // 状态里仍是本地那条
        assert_eq!(out["state"][0]["hlc"], "200-0-iphone");
    }

    #[test]
    fn sync_merge_is_idempotent() {
        let change = progress_change("100-0-iphone", 0.5);
        let once = merge(vec![], vec![change.clone()]);
        let twice = merge(once["state"].as_array().unwrap().clone(), vec![change]);
        assert_eq!(twice["effects"][0], "unchanged");
        assert_eq!(twice["conflictRate"], 0.0);
    }

    #[test]
    fn sync_merge_two_devices_converge() {
        let phone = progress_change("100-0-iphone", 0.3);
        let pad = progress_change("110-0-ipad", 0.7);
        let a = merge(vec![phone.clone()], vec![pad.clone()]);
        let b = merge(vec![pad], vec![phone]);
        assert_eq!(a["state"], b["state"], "两端必须收敛到同一状态");
    }

    #[test]
    fn sync_merge_rejects_mismatched_target() {
        // 进度值却带了 annotation 字段 → 目标与值类型不匹配
        let mut bad = progress_change("100-0-iphone", 0.5);
        bad["annotation"] = json!("22222222-2222-4222-8222-222222222222");
        let out = merge(vec![], vec![bad]);
        assert_eq!(out["error"]["code"], "SYNC_TARGET_MISMATCH");
    }

    #[test]
    fn sync_delta_walks_with_cursor() {
        let args = json!({ "state": [progress_change("100-0-iphone", 0.3)] }).to_string();
        let first = dispatch("sync.delta", &args);
        assert_eq!(first["changes"].as_array().unwrap().len(), 1);
        assert_eq!(first["nextCursor"], "100-0-iphone");
        assert_eq!(first["hasMore"], false);

        // 用上一页的游标再问 → 没有增量
        let args = json!({
            "state": json!([progress_change("100-0-iphone", 0.3)]),
            "since": "100-0-iphone",
        })
        .to_string();
        let second = dispatch("sync.delta", &args);
        assert_eq!(second["changes"].as_array().unwrap().len(), 0);
    }

    // ---------- 阶段②精修 ----------

    /// 造一个最小 EPUB（1 章 + 元数据 + EPUB3 目录 + 封面声明）。
    fn epub_fixture() -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buffer);
            let stored =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            let deflated = SimpleFileOptions::default();

            zip.start_file("mimetype", stored).unwrap();
            zip.write_all(b"application/epub+zip").unwrap();

            zip.start_file("META-INF/container.xml", deflated).unwrap();
            zip.write_all(r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#.as_bytes()).unwrap();

            zip.start_file("OEBPS/content.opf", deflated).unwrap();
            zip.write_all(
                r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">urn:uuid:ffi-test</dc:identifier>
    <dc:title>精修测试书</dc:title>
    <dc:creator>某作者</dc:creator>
    <dc:language>zh-CN</dc:language>
  </metadata>
  <manifest>
    <item id="c0" href="chap0.xhtml" media-type="application/xhtml+xml"/>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
  </manifest>
  <spine><itemref idref="c0"/></spine>
</package>"#
                    .as_bytes(),
            )
            .unwrap();

            zip.start_file("OEBPS/chap0.xhtml", deflated).unwrap();
            zip.write_all(
                r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body><h1>第一章</h1><p>春天的花开</p></body></html>"#
                    .as_bytes(),
            )
            .unwrap();

            zip.start_file("OEBPS/nav.xhtml", deflated).unwrap();
            zip.write_all(
                r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<body><nav epub:type="toc"><ol><li><a href="chap0.xhtml">第一章</a></li></ol></nav></body></html>"#
                    .as_bytes(),
            )
            .unwrap();

            zip.finish().unwrap();
        }
        buffer.into_inner()
    }

    #[test]
    fn jobs_refine_reads_metadata_and_toc() {
        let args = json!({ "bytes_hex": hex(&epub_fixture()) }).to_string();
        let out = dispatch("jobs.refine", &args);
        assert_eq!(out["title"], "精修测试书");
        assert_eq!(out["author"], "某作者");
        assert_eq!(out["language"], "zh-CN");
        assert_eq!(out["spineCount"], 1);
        assert_eq!(out["toc"][0], "第一章");
        assert!(out["charCount"].as_u64().unwrap() > 0);
    }

    #[test]
    fn jobs_refine_rejects_non_epub() {
        let args = json!({ "bytes_hex": hex(b"not a zip at all") }).to_string();
        let out = dispatch("jobs.refine", &args);
        assert_eq!(out["error"]["code"], "JOBS_EPUB");
    }

    // ---------- 句柄表：打开 / 搜索 / 关闭 ----------

    fn open_book() -> u64 {
        let args = json!({ "bytes_hex": hex(&epub_fixture()) }).to_string();
        let out = dispatch("book.open", &args);
        out["handle"]
            .as_u64()
            .unwrap_or_else(|| panic!("book.open 失败: {out}"))
    }

    #[test]
    fn book_open_reports_index_stats() {
        let handle = open_book();
        let args = json!({ "bytes_hex": hex(&epub_fixture()) }).to_string();
        let out = dispatch("book.open", &args);
        assert!(out["charCount"].as_u64().unwrap() > 0);
        assert_eq!(out["chapterCount"], 1);
        assert!(out["chunks"].as_u64().unwrap() >= 1);
        assert_eq!(out["truncated"], false);
        // 两次 open 拿到不同句柄
        let second = out["handle"].as_u64().unwrap();
        assert_ne!(handle, second);
        dispatch("book.close", &json!({ "handle": handle }).to_string());
        dispatch("book.close", &json!({ "handle": second }).to_string());
    }

    #[test]
    fn book_search_returns_locator_pointing_at_the_chapter() {
        let handle = open_book();
        let args = json!({ "handle": handle, "query": "春天的花开" }).to_string();
        let out = dispatch("book.search", &args);
        let hits = out["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "应当命中 1 条");
        let locator = &hits[0]["locator"];
        assert_eq!(locator["href"], "/OEBPS/chap0.xhtml");
        assert_eq!(locator["perch"]["format"], "epub");
        assert!(locator["locations"]["progression"].as_f64().is_some());
        // 粗定位：不给 CFI，精确位置由阅读器按文本重锚定
        assert!(locator["locations"].get("partialCfi").is_none());
        dispatch("book.close", &json!({ "handle": handle }).to_string());
    }

    #[test]
    fn book_search_miss_returns_empty() {
        let handle = open_book();
        let args = json!({ "handle": handle, "query": "完全不存在的词" }).to_string();
        let out = dispatch("book.search", &args);
        assert_eq!(out["hits"].as_array().unwrap().len(), 0);
        dispatch("book.close", &json!({ "handle": handle }).to_string());
    }

    #[test]
    fn closed_handle_is_rejected_not_misused() {
        let handle = open_book();
        let closed = dispatch("book.close", &json!({ "handle": handle }).to_string());
        assert_eq!(closed["closed"], true);
        // 再关一次是幂等的，不报错
        let again = dispatch("book.close", &json!({ "handle": handle }).to_string());
        assert_eq!(again["closed"], false);
        // 但用旧句柄搜索必须报错，而不是操作到别的书上
        let args = json!({ "handle": handle, "query": "春天" }).to_string();
        let out = dispatch("book.search", &args);
        assert_eq!(out["error"]["code"], "BOOK_BAD_HANDLE");
    }

    #[test]
    fn unknown_handle_is_rejected() {
        let args = json!({ "handle": 999_999, "query": "x" }).to_string();
        let out = dispatch("book.search", &args);
        assert_eq!(out["error"]["code"], "BOOK_BAD_HANDLE");
    }

    #[test]
    fn missing_handle_is_bad_args() {
        let out = dispatch("book.search", r#"{"query":"x"}"#);
        assert_eq!(out["error"]["code"], "FFI_BAD_ARGS");
    }

    #[test]
    fn registry_enforces_open_book_limit() {
        // 直接测 Registry 本身，不碰全局单例——否则会和并行跑的其他测试抢名额
        let mut local = Registry::default();
        let mut handles = Vec::new();
        for _ in 0..MAX_OPEN_BOOKS {
            let book = perch_jobs::extract_epub_text(
                &epub_fixture(),
                perch_jobs::ExtractOptions::default(),
            )
            .unwrap();
            let index = perch_jobs::index_book(&book, 64);
            handles.push(local.insert(OpenBook { text: book, index }).unwrap());
        }
        let book =
            perch_jobs::extract_epub_text(&epub_fixture(), perch_jobs::ExtractOptions::default())
                .unwrap();
        let index = perch_jobs::index_book(&book, 64);
        let error = local.insert(OpenBook { text: book, index }).unwrap_err();
        assert_eq!(error.code, "BOOK_TOO_MANY_OPEN");

        // 句柄互不相同且单调（永不复用）
        assert_eq!(handles.len(), MAX_OPEN_BOOKS);
        assert!(handles.windows(2).all(|pair| pair[0] < pair[1]));
    }

    // ---------- PDF 结构 ----------

    /// 造一个 n 页的最小 PDF，用于端到端验证 pdf.analyze。
    fn pdf_fixture(pages: usize) -> Vec<u8> {
        use lopdf::{Dictionary, Document, Object, dictionary};
        let mut document = Document::with_version("1.7");
        let pages_id = document.new_object_id();
        let mut page_ids = Vec::new();
        for _ in 0..pages {
            let contents = document.add_object(lopdf::Stream::new(Dictionary::new(), Vec::new()));
            let page = document.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
                "Contents" => contents,
            });
            page_ids.push(page);
        }
        let kids: Vec<Object> = page_ids.iter().map(|id| Object::Reference(*id)).collect();
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => i64::try_from(pages).unwrap(),
            }),
        );
        let info_id = document.add_object(dictionary! {
            "Title" => Object::string_literal("测试文档"),
        });
        document.trailer.set("Info", info_id);
        let catalog_id =
            document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        document.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn pdf_analyze_reports_pages_and_title() {
        let bytes = pdf_fixture(7);
        let args = json!({ "bytes_hex": hex(&bytes) }).to_string();
        let out = dispatch("pdf.analyze", &args);
        assert_eq!(out["pageCount"], 7);
        assert_eq!(out["title"], "测试文档");
        assert_eq!(out["version"], "1.7");
    }

    #[test]
    fn pdf_analyze_rejects_non_pdf() {
        let args = json!({ "bytes_hex": hex(b"definitely not pdf") }).to_string();
        let out = dispatch("pdf.analyze", &args);
        assert_eq!(out["error"]["code"], "PDF_NOT_PDF");
    }

    #[test]
    fn pdf_analyze_rejects_empty() {
        let out = dispatch("pdf.analyze", r#"{"bytes_hex":""}"#);
        assert_eq!(out["error"]["code"], "PDF_EMPTY");
    }

    #[test]
    fn sync_delta_rejects_bad_cursor() {
        let args = json!({ "state": [], "since": "不是HLC" }).to_string();
        let out = dispatch("sync.delta", &args);
        assert_eq!(out["error"]["code"], "SYNC_BAD_HLC");
    }
}
