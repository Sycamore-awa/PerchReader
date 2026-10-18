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

  /// 分析纯文本（编码探测 + 统计）——**只给导入阶段用**。
  ///
  /// ⚠️ 它要 `bytes_hex`，而且把**全文**当 JSON 字符串吐回来：
  /// 一个 10MB 的 TXT 在边界上要变成 20MB 的十六进制串再回来 10MB 文本。
  /// 阅读路径请用 [openTxt] + [TxtHandle.slice]（`docs/11` §5.2 ①）。
  Map<String, dynamic> analyzeText(List<int> bytes) =>
      invoke('txt.analyze', {'bytes_hex': _hex(bytes)});

  /// 打开一个 TXT 文件用于阅读（句柄化，**不建倒排索引**）。
  ///
  /// [encoding] 传 `'gbk'` / `'big5'` 之类可以跳过探测（对很短的样本，
  /// `chardetng` 的猜测没把握）；不传就走 BOM → 合法 UTF-8 → 猜测。
  ///
  /// 句柄由调用方负责释放（[TxtHandle.close]）。同时最多 4 个，与书索引的
  /// 配额**分开计数**——后台建索引任务再多也挤不掉阅读句柄。
  TxtHandle openTxt(String path, {String? encoding}) {
    final result = invoke('txt.open', {
      'path': path,
      if (encoding != null) 'encoding': encoding,
    });
    return TxtHandle._(
      this,
      result['handle'] as int,
      chars: result['chars'] as int,
      lines: result['lines'] as int,
      encoding: result['encoding'] as String,
      certain: result['certain'] as bool? ?? false,
      hadErrors: result['hadErrors'] as bool? ?? false,
    );
  }

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

  /// 打开一本书：抽正文 + 建索引，返回句柄。
  ///
  /// 索引是有状态的重对象，句柄由本包托管：**用完必须 [closeBook]**。
  /// 同时最多打开 8 本，超出会报 `BOOK_TOO_MANY_OPEN`。
  /// `format` 同 [refineBook]：知道扩展名时请显式传。
  /// `bytes` 同理必须是文件原始字节（见 [refineBook] 的 ⚠️）。
  Map<String, dynamic> openBook(
    List<int> bytes, {
    int? chunkChars,
    String? format,
  }) =>
      invoke('book.open', {
        'bytes_hex': _hex(bytes),
        if (chunkChars != null) 'chunk_chars': chunkChars,
        if (format != null) 'format': format,
      });

  /// 在已打开的索引里检索。
  ///
  /// 每条命中带一个**粗定位** `locator`（章节 href + 全书进度 + 命中原文）：
  /// 精确位置需要阅读器在该章内按文本重锚定（见 `docs/02` §五）。
  Map<String, dynamic> searchBook({
    required int handle,
    required String query,
    int? limit,
  }) =>
      invoke('book.search', {
        'handle': handle,
        'query': query,
        if (limit != null) 'limit': limit,
      });

  /// 关闭一本书的索引并释放内存。对已关闭的句柄调用是安全的（返回 false）。
  Map<String, dynamic> closeBook(int handle) =>
      invoke('book.close', {'handle': handle});

  /// 阶段②精修：解析元数据 / 封面 / 目录 / 字数（EPUB 与 TXT，PDF 不支持）。
  ///
  /// ⚠️ `bytes` 必须是**文件原始字节**（UTF-8 编码的文本文件就是 UTF-8 字节）。
  /// 不要传 `String.codeUnits` —— 那是 UTF-16 码元，纯 ASCII 时碰巧一样，
  /// 一遇中文 Rust 侧收到的就是乱码，而且**不会报错**，只会安静地建出错误的索引。
  /// 文本请用 `utf8.encode(text)`。
  ///
  /// 返回 `{title?, author?, language?, coverHref?, toc, spineCount, charCount,
  /// encoding?}`。
  ///
  /// - `maxChars` 限制抽取正文的字符数（超限会被截断并置 `truncated`，不会 OOM）
  /// - `format` 取 `'epub'` / `'txt'` / `'pdf'`。**上层知道扩展名时应当传**：
  ///   不传就按内容嗅探，而嗅探会把一个损坏的 EPUB 当成纯文本收下。
  Map<String, dynamic> refineBook(
    List<int> bytes, {
    int? maxChars,
    String? format,
  }) =>
      invoke('jobs.refine', {
        'bytes_hex': _hex(bytes),
        if (maxChars != null) 'max_chars': maxChars,
        if (format != null) 'format': format,
      });

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

/// 一屏文本。
class TxtSlice {
  const TxtSlice({
    required this.text,
    required this.start,
    required this.end,
    required this.total,
  });

  /// 文本本身。
  final String text;

  /// 起始字符偏移（含）。
  final int start;

  /// 结束字符偏移（不含）。
  final int end;

  /// 全书总字符数（用来画进度条）。
  final int total;

  /// 本屏字符数。
  int get chars => end - start;

  @override
  String toString() => 'TxtSlice($start..$end of $total, ${text.length} 字符)';
}

/// 一行在全文里的位置。
class TxtLine {
  const TxtLine({required this.line, this.start, this.end, required this.totalLines});

  /// 0 基行号。
  final int line;

  /// 该行的起始字符偏移（取不到时为 null）。
  final int? start;

  /// 该行的结束字符偏移（不含）。
  final int? end;

  /// 总行数。
  final int totalLines;
}

/// 一个已打开的 TXT 文档。
///
/// **生命周期归调用方：用完必须 [close]**。句柄在 Rust 侧自增且永不复用，
/// 所以"关掉之后又拿来用"只会得到明确的 `TXT_BAD_HANDLE`，不会误操作到别的书。
class TxtHandle {
  TxtHandle._(
    this._core,
    this.handle, {
    required this.chars,
    required this.lines,
    required this.encoding,
    required this.certain,
    required this.hadErrors,
  });

  final PerchCore _core;

  /// Rust 侧的句柄。一般不用直接碰它。
  final int handle;

  /// 全书字符数（**不是字节数**：中文一个字符 3 字节）。
  final int chars;

  /// 全书行数。
  final int lines;

  /// 实际使用的编码名（如 `UTF-8` / `GBK`）。
  final String encoding;

  /// 编码是否**确定**：为 false 说明是猜测，UI 应当提供"换个编码重读"。
  final bool certain;

  /// 解码时是否出现过非法序列（出现即说明编码可能选错了）。
  final bool hadErrors;

  bool _closed = false;

  /// 取一屏文本。
  ///
  /// [start] / [chars] 都是**字符**数。`chars` 有上限（2 万），
  /// 超过会抛 `TXT_SLICE_BAD_SIZE`——这是故意的：一次取整本书是调用方的 bug，
  /// 早点炸比悄悄传一个 10MB 字符串好。末页不足一屏是正常的，按实际长度返回。
  TxtSlice slice(int start, int chars) {
    _ensureOpen('slice');
    final result = _core.invoke('txt.slice', {
      'handle': handle,
      'start': start,
      'chars': chars,
    });
    return TxtSlice(
      text: result['text'] as String,
      start: result['start'] as int,
      end: result['end'] as int,
      total: result['total'] as int,
    );
  }

  /// 字符偏移 → Locator（进度落库、跨端同步都用它）。
  Map<String, dynamic> locate(int charOffset) {
    _ensureOpen('locate');
    final result = _core.invoke('txt.locate', {
      'handle': handle,
      'char': charOffset,
    });
    return (result['locator'] as Map).cast<String, dynamic>();
  }

  /// Locator → 字符偏移。
  ///
  /// 返回 `null` 表示**这个 Locator 在本文件里定位不出来**——这不是错误，
  /// 调用方应当退回 `locations.progression`（Rust 侧的三级回退链已经在内部试过
  /// 字符偏移 → 字节偏移 → 进度）。
  int? offsetOf(Map<String, dynamic> locator) {
    _ensureOpen('offsetOf');
    final result = _core.invoke('txt.offset', {
      'handle': handle,
      'locator': locator,
    });
    return result['char'] as int?;
  }

  /// 字符偏移 → 行号与该行的字符区间（排版、目录、跳转都要用）。
  TxtLine lineAt(int charOffset) {
    _ensureOpen('lineAt');
    final result = _core.invoke('txt.line', {
      'handle': handle,
      'char': charOffset,
    });
    return TxtLine(
      line: result['line'] as int,
      start: result['start'] as int?,
      end: result['end'] as int?,
      totalLines: result['totalLines'] as int,
    );
  }

  /// 释放。（重复调用是安全的。）
  void close() {
    if (_closed) {
      return;
    }
    _closed = true;
    _core.invoke('txt.close', {'handle': handle});
  }

  void _ensureOpen(String method) {
    if (_closed) {
      throw StateError(
        'TxtHandle.$method：这个句柄已经 close 过了（句柄 $handle）。'
        '句柄在核心侧自增且永不复用，请重新 openTxt。',
      );
    }
  }

  @override
  String toString() =>
      'TxtHandle($handle, $encoding, $chars 字符/$lines 行'
      '${certain ? '' : ', 编码为猜测'})';
}
