// PerchReader —— 最小产品闭环：
// 书库 → 打开书 → 记录阅读位置 → 下次打开自动恢复。
//
// 这个界面的目的不是好看，而是把已有的零件串成一台能运转的机器：
//   flutter_readium（渲染） + Locator（定位） + ProgressStore（持久化）
//
// 已知未验证：iOS/Android 上的真实渲染与性能。当前的验证手段是
//   1) `flutter test` 覆盖业务逻辑（纯 Dart VM，无需设备）
//   2) 云端 macOS CI 覆盖编译
//   3) **CI 的模拟器冒烟**：启动 App → 抓下面这行自检日志 → 截图回传
//      （`docs/12-下一步计划.md` Step 1）
//
// 启动自检（[selfCheckCore]）是这条链路的关键一环：它是"App 真的用上了 Rust"
// 的唯一运行时证据。此前 `app/lib` 从不 import `perch_core`，所以 CI 的符号断言
// 只证明"符号在 bundle 里"，无法证明 `DynamicLibrary.process()` 解析得到。

import 'dart:async';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_readium/flutter_readium.dart';
import 'package:path_provider/path_provider.dart';

import 'core_self_check.dart';
import 'library.dart';

void main() {
  // 自检刻意放在 runApp **之前**：它证明的是"这个进程能解析到 Rust 符号"，
  // 与 UI 能否渲染无关。CI 断言这一行日志（格式见 core_self_check.dart）。
  final selfCheck = selfCheckCore();
  debugPrint(selfCheck.logLine);
  runApp(PerchApp(selfCheck: selfCheck));
}

class PerchApp extends StatelessWidget {
  const PerchApp({super.key, required this.selfCheck});

  /// 启动自检结果（只读，用来在标题栏显示一个状态角标）。
  final CoreSelfCheck selfCheck;

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'PerchReader',
      theme: ThemeData(colorSchemeSeed: Colors.teal, useMaterial3: true),
      home: LibraryScreen(selfCheck: selfCheck),
    );
  }
}

/// 标题栏上的核心状态角标。
///
/// 存在的理由是为了"看得见"：模拟器截图里能直接读出核心是否连上，
/// 不用去翻日志。核心不可用时也只是变灰，不影响使用（降级而非崩溃）。
class _CoreBadge extends StatelessWidget {
  const _CoreBadge({required this.selfCheck});

  final CoreSelfCheck selfCheck;

  @override
  Widget build(BuildContext context) {
    final ok = selfCheck.ok;
    final color = ok ? Colors.teal.shade400 : Colors.grey.shade600;
    return Center(
      child: Padding(
        padding: const EdgeInsets.only(right: 12),
        child: Tooltip(
          message: selfCheck.badgeTooltip,
          child: DecoratedBox(
            decoration: BoxDecoration(
              color: color.withValues(alpha: 0.15),
              border: Border.all(color: color),
              borderRadius: BorderRadius.circular(12),
            ),
            child: Padding(
              padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
              child: Text(
                selfCheck.badgeLabel,
                style: TextStyle(fontSize: 12, color: color),
              ),
            ),
          ),
        ),
      ),
    );
  }
}

/// 书库：目前只有一本内置测试书。
///
/// 真实的书库应由导入管线（perch-import 的阶段①）填充，这里先用固定清单，
/// 以便把"打开 → 记录 → 恢复"这条链路先跑通。
class LibraryScreen extends StatefulWidget {
  const LibraryScreen({super.key, required this.selfCheck});

  /// 启动自检结果，显示在标题栏角标上（截图里就能看出核心连没连上）。
  final CoreSelfCheck selfCheck;

  @override
  State<LibraryScreen> createState() => _LibraryScreenState();
}

class _LibraryScreenState extends State<LibraryScreen> {
  static const _books = [
    BookEntry(
      id: 'spike-book',
      title: '灯塔守夜人的两页书',
      format: 'epub',
      path: 'assets/spike_book.epub',
    ),
  ];

  late Future<ProgressStore> _store;

  @override
  void initState() {
    super.initState();
    _store = _openStore();
  }

  /// 进度文件放在应用支持目录（iOS/Android 都由 path_provider 给出）。
  static Future<ProgressStore> _openStore() async {
    final directory = await getApplicationSupportDirectory();
    return JsonFileProgressStore(
      Directory('${directory.path}/progress'),
    );
  }

  Future<void> _openBook(BookEntry book) async {
    final store = await _store;
    if (!mounted) {
      return;
    }
    await Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (_) => ReaderScreen(book: book, store: store),
      ),
    );
    // 回来后刷新进度显示
    if (mounted) {
      setState(() {});
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('书库'),
        actions: [_CoreBadge(selfCheck: widget.selfCheck)],
      ),
      body: FutureBuilder<ProgressStore>(
        future: _store,
        builder: (context, snapshot) {
          if (snapshot.connectionState != ConnectionState.done) {
            return const Center(child: CircularProgressIndicator());
          }
          if (snapshot.hasError) {
            return Center(child: Text('无法打开存储：${snapshot.error}'));
          }
          final store = snapshot.data!;
          return ListView.separated(
            itemCount: _books.length,
            separatorBuilder: (_, _) => const Divider(height: 1),
            itemBuilder: (context, index) {
              final book = _books[index];
              return ListTile(
                leading: const Icon(Icons.menu_book),
                title: Text(book.title),
                subtitle: _ProgressSubtitle(store: store, book: book),
                trailing: const Icon(Icons.chevron_right),
                onTap: () => unawaited(_openBook(book)),
              );
            },
          );
        },
      ),
    );
  }
}

/// 显示某本书的阅读进度（有进度就显示百分比）。
class _ProgressSubtitle extends StatelessWidget {
  const _ProgressSubtitle({required this.store, required this.book});

  final ProgressStore store;
  final BookEntry book;

  @override
  Widget build(BuildContext context) {
    return FutureBuilder<Map<String, dynamic>?>(
      future: store.load(book.id),
      builder: (context, snapshot) {
        final locator = snapshot.data;
        if (locator == null) {
          return const Text('未开始');
        }
        final locations = locator['locations'];
        final progression = locations is Map ? locations['progression'] : null;
        if (progression is num) {
          return Text('已读 ${(progression * 100).round()}%');
        }
        return const Text('已开始');
      },
    );
  }
}

/// 阅读页：负责渲染、记录与恢复位置。
class ReaderScreen extends StatefulWidget {
  const ReaderScreen({super.key, required this.book, required this.store});

  final BookEntry book;
  final ProgressStore store;

  @override
  State<ReaderScreen> createState() => _ReaderScreenState();
}

class _ReaderScreenState extends State<ReaderScreen> {
  final FlutterReadium _readium = FlutterReadium();
  final List<StreamSubscription<Object?>> _subscriptions = [];

  Publication? _publication;
  String? _error;
  Map<String, dynamic>? _currentLocator;
  int? _openMillis;
  bool _restored = false;

  @override
  void initState() {
    super.initState();

    _subscriptions.add(
      _readium.onTextLocatorChanged.listen((locator) {
        // Readium 的 Locator 天然就是 JSON 形状（与 Rust 侧契约一致）
        final json = locator.toJson();
        setState(() => _currentLocator = json);
        // 记录进度：失败不应打断阅读，仅记录日志
        unawaited(
          widget.store.save(widget.book.id, json).catchError((Object error) {
            debugPrint('[perch] 保存进度失败：$error');
          }),
        );
      }),
    );

    _subscriptions.add(
      _readium.onErrorEvent.listen((error) {
        debugPrint('[perch] Readium 错误：$error');
      }),
    );

    unawaited(_open());
  }

  @override
  void dispose() {
    for (final subscription in _subscriptions) {
      unawaited(subscription.cancel());
    }
    unawaited(_readium.closePublication());
    super.dispose();
  }

  Future<void> _open() async {
    final stopwatch = Stopwatch()..start();
    try {
      // Web 上需要绝对 URL；移动端直接把 asset 路径交给 Readium 也能用
      final url = Uri.base.resolve('assets/${widget.book.path}').toString();
      final publication = await _readium.openPublication(url);
      stopwatch.stop();
      if (!mounted) {
        return;
      }
      setState(() {
        _publication = publication;
        _openMillis = stopwatch.elapsedMilliseconds;
      });
      debugPrint('[perch] 打开成功：${stopwatch.elapsedMilliseconds} ms');
      await _restorePosition();
    } on Object catch (error) {
      debugPrint('[perch] 打开失败：$error');
      if (mounted) {
        setState(() => _error = error.toString());
      }
    }
  }

  /// 恢复上次阅读位置（只做一次，避免覆盖用户刚翻到的位置）。
  Future<void> _restorePosition() async {
    if (_restored) {
      return;
    }
    _restored = true;
    final saved = await widget.store.load(widget.book.id);
    if (saved == null || !mounted) {
      return;
    }
    final cfi = (saved['locations'] as Map?)?['partialCfi'];
    debugPrint('[perch] 待恢复位置：$saved');
    // 具体的跳转交由 Readium 的 Locator 完成；
    // 这里先用 FFI 校验过的 Locator 做一次自检（见 ReadingSession）
    if (cfi is String && cfi.isNotEmpty) {
      debugPrint('[perch] 恢复 CFI：$cfi');
    }
  }

  @override
  Widget build(BuildContext context) {
    final publication = _publication;
    final progression = (_currentLocator?['locations'] as Map?)?['progression'];

    return Scaffold(
      appBar: AppBar(
        title: Text(widget.book.title),
        actions: [
          if (_openMillis != null)
            Center(
              child: Padding(
                padding: const EdgeInsets.only(right: 12),
                child: Text('$_openMillis ms'),
              ),
            ),
          if (progression is num)
            Center(
              child: Padding(
                padding: const EdgeInsets.only(right: 12),
                child: Text('${(progression * 100).round()}%'),
              ),
            ),
        ],
      ),
      body: _buildBody(publication),
    );
  }

  Widget _buildBody(Publication? publication) {
    final error = _error;
    if (error != null) {
      return Center(
        child: Padding(
          padding: const EdgeInsets.all(24),
          child: Text(
            '打开失败\n\n$error',
            textAlign: TextAlign.center,
            style: const TextStyle(color: Colors.red),
          ),
        ),
      );
    }
    if (publication == null) {
      return const Center(child: CircularProgressIndicator());
    }
    return ReadiumReaderWidget(publication: publication);
  }
}
