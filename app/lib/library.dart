// 书库与阅读进度的最小产品逻辑。
//
// 设计上的两个关键取舍：
//
// 1. **进度存储抽象成接口**（[ProgressStore]），当前给一个 JSON 文件实现。
//    最终应按 docs/02 用 SQLite（Dart 侧 sqflite/drift），但接口不变。
//    用文件实现的另一个好处：可以在纯 Dart VM 里测，不需要设备。
//
// 2. **Locator 校验是可注入的**（[LocatorValidator]），生产环境传核心的
//    `PerchCore.validateLocator`（走 FFI 到 Rust），测试里传假实现。
//    这样"非法 Locator 不得入库"这条规则能被单测覆盖，而不用真的加载 .so。
//
// 一条刻意的规则：**写入前必须校验**。同步来的、别处读来的 Locator 都可能已经坏了，
// 坏数据一旦落库就会在下次启动时炸在更远的地方。

import 'dart:convert';
import 'dart:io';

/// 一本书的元信息（阶段①登记的产物）。
class BookEntry {
  const BookEntry({
    required this.id,
    required this.title,
    required this.format,
    required this.path,
  });

  /// 唯一标识（阶段①由 Rust 侧生成 UUID）。
  final String id;

  /// 显示标题。
  final String title;

  /// 格式：`epub` / `pdf` / `txt`。
  final String format;

  /// 内容位置：asset key 或本地文件路径。
  final String path;

  Map<String, dynamic> toJson() => {
    'id': id,
    'title': title,
    'format': format,
    'path': path,
  };

  factory BookEntry.fromJson(Map<String, dynamic> json) => BookEntry(
    id: json['id'] as String,
    title: json['title'] as String,
    format: json['format'] as String,
    path: json['path'] as String,
  );
}

/// Locator 校验器：非法时抛异常。
typedef LocatorValidator = void Function(Map<String, dynamic> locator);

/// 阅读进度存储。
abstract interface class ProgressStore {
  /// 读回某本书的进度；没有则返回 `null`。
  Future<Map<String, dynamic>?> load(String bookId);

  /// 写入进度（**调用方负责先校验**）。
  Future<void> save(String bookId, Map<String, dynamic> locator);

  /// 清除进度。
  Future<void> clear(String bookId);
}

/// 基于 JSON 文件的实现。
///
/// 每本书一个文件（`progress_<id>.json`），整体覆写。
/// 对这个数据量（每本一条记录）足够；换成 SQLite 时只需换实现。
class JsonFileProgressStore implements ProgressStore {
  JsonFileProgressStore(this.directory);

  /// 存放进度文件的目录（测试里用临时目录）。
  final Directory directory;

  File _fileFor(String bookId) =>
      File('${directory.path}/progress_$bookId.json');

  @override
  Future<Map<String, dynamic>?> load(String bookId) async {
    final file = _fileFor(bookId);
    if (!await file.exists()) {
      return null;
    }
    try {
      final text = await file.readAsString();
      final decoded = jsonDecode(text);
      return decoded is Map<String, dynamic> ? decoded : null;
    } on Object {
      // 文件损坏不应让 App 打不开书：当作没有进度处理
      return null;
    }
  }

  @override
  Future<void> save(String bookId, Map<String, dynamic> locator) async {
    await directory.create(recursive: true);
    // 先写临时文件再改名：避免写到一半崩溃留下半个 JSON
    final temporary = File('${directory.path}/progress_$bookId.json.tmp');
    await temporary.writeAsString(jsonEncode(locator), flush: true);
    await temporary.rename(_fileFor(bookId).path);
  }

  @override
  Future<void> clear(String bookId) async {
    final file = _fileFor(bookId);
    if (await file.exists()) {
      await file.delete();
    }
  }
}

/// 阅读会话：把"打开书 / 记录进度 / 恢复进度"串起来。
///
/// 这是最小产品闭环的业务核心，刻意不依赖 Flutter，因此可被单测完整覆盖。
class ReadingSession {
  ReadingSession({
    required this.store,
    LocatorValidator? validator,
  }) : _validator = validator; // ignore: prefer_initializing_formals 私有字段无法用 this._ 语法

  /// 进度存储。
  final ProgressStore store;

  final LocatorValidator? _validator;

  /// 恢复上次阅读位置。
  ///
  /// 读回的内容会再校验一次：数据可能在别处被改坏（多设备同步、手工编辑）。
  /// 校验失败就当作没有进度，而不是抛给 UI。
  Future<Map<String, dynamic>?> restore(BookEntry book) async {
    final saved = await store.load(book.id);
    if (saved == null) {
      return null;
    }
    final validator = _validator;
    if (validator != null) {
      try {
        validator(saved);
      } on Object {
        return null;
      }
    }
    return saved;
  }

  /// 记录阅读位置。
  ///
  /// 非法 Locator 直接抛异常、**不写入**——坏数据不该进入持久化层。
  Future<void> record(BookEntry book, Map<String, dynamic> locator) async {
    _validator?.call(locator);
    await store.save(book.id, locator);
  }
}
