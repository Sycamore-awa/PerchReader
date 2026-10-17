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

        other => Err(FfiError::new(
            "FFI_UNKNOWN_METHOD",
            format!("未知方法: {other}"),
        )),
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
}
