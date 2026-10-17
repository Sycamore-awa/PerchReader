// PerchReader 核心的 Dart 绑定。
//
// 契约（与 core/perch-ffi 对应）：
//   perch_call(method, args_json) -> result_json   （返回堆字符串，需释放）
//   perch_string_free(ptr)
//
// 各平台加载方式不同：
//   - iOS      ：静态库链进 App，符号在进程内 → DynamicLibrary.process()
//   - Android  ：链接为 .so，随 APK 打包 → DynamicLibrary.open('libperch_ffi.so')
//   - macOS/Linux 桌面 / 本机测试：DynamicLibrary.open(具体路径)
//
// 说明：本文件只用 `dart:ffi`，不依赖 flutter，因此可以在**纯 Dart 环境**里
// 跑端到端测试（见 tool/ffi_smoke.dart）——这是没有 Mac 时唯一能真正验证
// FFI 边界的手段。

import 'dart:convert';
import 'dart:ffi';
import 'dart:io';

import 'package:ffi/ffi.dart';

/// `perch_call` 的 C 签名。
typedef _PerchCallNative = Pointer<Utf8> Function(
  Pointer<Uint8> methodPtr,
  IntPtr methodLen,
  Pointer<Uint8> argsPtr,
  IntPtr argsLen,
);

typedef _PerchCallDart = Pointer<Utf8> Function(
  Pointer<Uint8> methodPtr,
  int methodLen,
  Pointer<Uint8> argsPtr,
  int argsLen,
);

typedef _FreeNative = Void Function(Pointer<Utf8> ptr);
typedef _FreeDart = void Function(Pointer<Utf8> ptr);

/// 核心抛出的业务错误。
class PerchCoreException implements Exception {
  PerchCoreException(this.code, this.message);

  /// 稳定错误码（如 `MODEL_EMPTY_FIELD`、`FFI_BAD_ARGS`）。
  final String code;

  /// 人类可读信息。
  final String message;

  @override
  String toString() => 'PerchCoreException($code): $message';
}

/// 核心的 Dart 门面。
class PerchCore {
  PerchCore._(this._call, this._free);

  final _PerchCallDart _call;
  final _FreeDart _free;

  /// 按平台打开核心库。
  ///
  /// [libraryPath] 只在桌面/测试时用得到；移动端会走默认路径。
  static PerchCore open({String? libraryPath}) {
    final library = _openLibrary(libraryPath);
    return PerchCore._(
      library.lookupFunction<_PerchCallNative, _PerchCallDart>('perch_call'),
      library.lookupFunction<_FreeNative, _FreeDart>('perch_string_free'),
    );
  }

  static DynamicLibrary _openLibrary(String? libraryPath) {
    if (libraryPath != null) {
      return DynamicLibrary.open(libraryPath);
    }
    if (Platform.isIOS || Platform.isMacOS) {
      // iOS：静态链接后符号在进程内
      return DynamicLibrary.process();
    }
    if (Platform.isAndroid) {
      return DynamicLibrary.open('libperch_ffi.so');
    }
    // 桌面 Linux/Windows
    return DynamicLibrary.open(
      Platform.isLinux ? 'libperch_ffi.so' : 'perch_ffi.dll',
    );
  }

  /// 调用一个方法，返回已解析的 JSON。
  ///
  /// 出错时抛 [PerchCoreException]（错误码来自 Rust 侧，稳定可判断）。
  Map<String, dynamic> invoke(String method, [Map<String, dynamic>? args]) {
    final methodBytes = utf8.encode(method);
    final argsBytes = utf8.encode(jsonEncode(args ?? const {}));

    final methodPtr = malloc<Uint8>(methodBytes.length);
    final argsPtr = malloc<Uint8>(argsBytes.length);
    try {
      methodPtr.asTypedList(methodBytes.length).setAll(0, methodBytes);
      argsPtr.asTypedList(argsBytes.length).setAll(0, argsBytes);

      final resultPtr = _call(
        methodPtr,
        methodBytes.length,
        argsPtr,
        argsBytes.length,
      );
      if (resultPtr == nullptr) {
        throw PerchCoreException('FFI_NULL', '$method 返回了空指针');
      }
      try {
        final text = resultPtr.toDartString();
        final decoded = jsonDecode(text);
        if (decoded is! Map<String, dynamic>) {
          throw PerchCoreException('FFI_BAD_JSON', '$method 返回的不是对象');
        }
        final error = decoded['error'];
        if (error is Map && error['code'] is String) {
          throw PerchCoreException(
            error['code'] as String,
            (error['message'] ?? '').toString(),
          );
        }
        return decoded;
      } finally {
        _free(resultPtr);
      }
    } finally {
      malloc.free(methodPtr);
      malloc.free(argsPtr);
    }
  }

  // ---- 便捷封装（按需增长） ----

  /// 核心版本。
  String version() => invoke('version')['core'] as String;

  /// 校验一个 Locator；非法时抛异常。
  void validateLocator(Map<String, dynamic> locator) =>
      invoke('locator.validate', locator);

  /// 计算文件指纹。
  String fingerprint({
    required int size,
    required List<int> head,
    required List<int> tail,
  }) =>
      invoke('fingerprint', {
        'size': size,
        'head_hex': _hex(head),
        'tail_hex': _hex(tail),
      })['fingerprint'] as String;

  /// 分析纯文本（编码探测 + 统计）。
  Map<String, dynamic> analyzeText(List<int> bytes) =>
      invoke('txt.analyze', {'bytes_hex': _hex(bytes)});

  /// 全文检索。
  List<Map<String, dynamic>> search(
    String text,
    String query, {
    int? chunkChars,
  }) {
    final result = invoke('index.search', {
      'text': text,
      'query': query,
      if (chunkChars != null) 'chunk_chars': chunkChars,
    });
    return (result['hits'] as List).cast<Map<String, dynamic>>();
  }

  /// 解析 PDF 结构（页数 / 大纲 / 元数据）。
  ///
  /// 只返回结构信息，**不抽正文、不渲染**：正文走平台注入的 port。
  /// 返回 `{pageCount, version, title?, author?, subject?, producer?, outline?}`。
  Map<String, dynamic> analyzePdf(List<int> bytes) =>
      invoke('pdf.analyze', {'bytes_hex': _hex(bytes)});

  /// 合并本地状态与远端变更。
  ///
  /// 返回 `{state, stats, conflictRate, effects}`：
  /// `state` 是合并后的寄存器快照（可直接落盘），`conflictRate` 是
  /// 「被丢弃的字段级变更 / 总字段级变更」。
  Map<String, dynamic> mergeSync({
    required List<Map<String, dynamic>> local,
    required List<Map<String, dynamic>> remote,
  }) =>
      invoke('sync.merge', {'local': local, 'remote': remote});

  /// 增量同步：给定寄存器快照与游标，取一页变更。
  ///
  /// `since` 为空表示首次同步。返回 `{changes, nextCursor, hasMore}`，
  /// 下次请求把 `nextCursor` 原样传回来即可。
  Map<String, dynamic> deltaSync({
    required List<Map<String, dynamic>> state,
    String? since,
    int? limit,
  }) =>
      invoke('sync.delta', {
        'state': state,
        if (since != null) 'since': since,
        if (limit != null) 'limit': limit,
      });

  static String _hex(List<int> bytes) {
    final buffer = StringBuffer();
    for (final byte in bytes) {
      buffer.write(byte.toRadixString(16).padLeft(2, '0'));
    }
    return buffer.toString();
  }
}
