import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_readium/flutter_readium.dart';

void main() {
  runApp(const PerchSpikeApp());
}

class PerchSpikeApp extends StatelessWidget {
  const PerchSpikeApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Perch Spike',
      theme: ThemeData(colorSchemeSeed: Colors.teal, useMaterial3: true),
      home: const ReaderScreen(),
    );
  }
}

/// go/no-go spike：用 `flutter_readium` 打开一本**本仓库自产的中文 EPUB**，
/// 并把 Readium 给出的 `Locator` 原样打印出来。
///
/// 验证目标（对应 `docs/06-UI技术栈决策.md` §六 的清单）：
///
/// 1. 中文 EPUB 能否正确渲染（排版、断行、字体回退）
/// 2. 能否翻页，帧率如何
/// 3. **能否拿到 Locator**，以及里面是否带 CFI / progression
///    —— 这是"进度跨端同步"与"标注不漂移"的地基
/// 4. 打开耗时（目标 < 1s）
///
/// 打印完整 JSON 而不是挑字段，是因为本次 spike 的另一半目的是
/// **核对 Readium 的 Locator 结构与 `perch-model::Locator` 是否真的对得上**
/// （若对得上，两边的互转就退化成字段拷贝）。
class ReaderScreen extends StatefulWidget {
  const ReaderScreen({super.key});

  @override
  State<ReaderScreen> createState() => _ReaderScreenState();
}

class _ReaderScreenState extends State<ReaderScreen> {
  /// 与 `pubspec.yaml` 里登记的 assets 路径一致。
  /// Web 上 Flutter 会把 asset 放在 `assets/<key>` 下，所以最终 URL 是 `assets/assets/...`。
  static const String _assetKey = 'assets/spike_book.epub';

  final FlutterReadium _readium = FlutterReadium();
  final List<StreamSubscription<Object?>> _subscriptions = [];
  final List<String> _log = [];

  Publication? _publication;
  String? _error;
  String _locatorJson = '(等待 Locator 事件…)';
  int? _openMillis;
  bool _showInspector = true;

  @override
  void initState() {
    super.initState();

    _subscriptions.add(
      _readium.onTextLocatorChanged.listen((locator) {
        final pretty = const JsonEncoder.withIndent('  ').convert(locator.toJson());
        setState(() => _locatorJson = pretty);
        // 完整 JSON 打到控制台：本次 spike 要核对的核心数据
        // （Readium 的 Locator 里到底有没有 CFI、progression 等字段）
        debugPrint('[perch-spike] Locator JSON:\n$pretty');
        _append('Locator 更新 → ${locator.href}');
      }),
    );

    _subscriptions.add(
      _readium.onErrorEvent.listen((error) => _append('Readium 错误：$error')),
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

  void _append(String message) {
    // 同时打到浏览器/设备控制台：这样无头浏览器（CI）也能拿到验证结果，
    // 不必依赖人工看图。
    debugPrint('[perch-spike] $message');
    setState(() {
      _log.insert(0, message);
      if (_log.length > 40) {
        _log.removeLast();
      }
    });
  }

  Future<void> _open() async {
    final stopwatch = Stopwatch()..start();
    try {
      // Web 上 Flutter 把 asset 放在 `assets/<key>`；但 Readium 内部会用
      // `new URL(...)` 解析，必须给**绝对 URL**（相对路径会报 Invalid base URL），
      // 所以用 Uri.base 解析一次。
      final url = Uri.base.resolve('assets/$_assetKey').toString();
      _append('URL：$url');
      final publication = await _readium.openPublication(url);
      stopwatch.stop();
      if (!mounted) {
        return;
      }
      setState(() {
        _publication = publication;
        _openMillis = stopwatch.elapsedMilliseconds;
      });
      _append('打开成功：${stopwatch.elapsedMilliseconds} ms');
      _append('标题：${publication.metadata.title}');
      _append('阅读顺序条目数：${publication.readingOrder.length}');
    } on Object catch (error) {
      stopwatch.stop();
      _append('打开失败：$error');
      if (mounted) {
        setState(() => _error = error.toString());
      }
    }
  }

  Future<void> _step({required bool forward}) async {
    try {
      if (forward) {
        await _readium.goForward();
      } else {
        await _readium.goBackward();
      }
    } on Object catch (error) {
      _append('翻页失败：$error');
    }
  }

  @override
  Widget build(BuildContext context) {
    final publication = _publication;
    final error = _error;

    return Scaffold(
      appBar: AppBar(
        title: const Text('Perch Spike — flutter_readium'),
        actions: [
          if (_openMillis != null)
            Center(
              child: Padding(
                padding: const EdgeInsets.only(right: 12),
                child: Text('打开 $_openMillis ms'),
              ),
            ),
          IconButton(
            tooltip: _showInspector ? '隐藏检查器' : '显示检查器',
            icon: Icon(_showInspector ? Icons.visibility_off : Icons.visibility),
            onPressed: () => setState(() => _showInspector = !_showInspector),
          ),
        ],
      ),
      body: Column(
        children: [
          Expanded(child: _buildReader(error, publication)),
          if (_showInspector)
            _InspectorPanel(
              locatorJson: _locatorJson,
              log: _log,
              onBackward: () => unawaited(_step(forward: false)),
              onForward: () => unawaited(_step(forward: true)),
            ),
        ],
      ),
    );
  }

  Widget _buildReader(String? error, Publication? publication) {
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

/// 底部检查器：翻页按钮 + Readium 给出的 Locator 原始 JSON + 事件日志。
class _InspectorPanel extends StatelessWidget {
  const _InspectorPanel({
    required this.locatorJson,
    required this.log,
    required this.onBackward,
    required this.onForward,
  });

  final String locatorJson;
  final List<String> log;
  final VoidCallback onBackward;
  final VoidCallback onForward;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Container(
      height: 260,
      decoration: BoxDecoration(
        color: theme.colorScheme.surfaceContainerHighest,
        border: Border(top: BorderSide(color: theme.dividerColor)),
      ),
      child: Column(
        children: [
          Row(
            children: [
              IconButton(
                onPressed: onBackward,
                icon: const Icon(Icons.chevron_left),
                tooltip: '上一页',
              ),
              IconButton(
                onPressed: onForward,
                icon: const Icon(Icons.chevron_right),
                tooltip: '下一页',
              ),
              const Spacer(),
              Padding(
                padding: const EdgeInsets.only(right: 12),
                child: Text('Readium Locator（原始 JSON）', style: theme.textTheme.labelMedium),
              ),
            ],
          ),
          Expanded(
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Expanded(flex: 3, child: _Mono(text: locatorJson)),
                const VerticalDivider(width: 1),
                Expanded(flex: 2, child: _Mono(text: log.join('\n'))),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

class _Mono extends StatelessWidget {
  const _Mono({required this.text});

  final String text;

  @override
  Widget build(BuildContext context) {
    return SingleChildScrollView(
      padding: const EdgeInsets.all(12),
      child: Text(
        text,
        style: const TextStyle(
          fontFamily: 'monospace',
          fontSize: 11,
          height: 1.4,
        ),
      ),
    );
  }
}
