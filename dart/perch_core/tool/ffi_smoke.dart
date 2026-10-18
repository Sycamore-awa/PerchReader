// FFI 边界的端到端冒烟测试（**纯 Dart，不需要 Flutter 设备**）。
//
// 用法：
//   # 1) 先编出本机的动态库
//   cd core && cargo build -p perch-ffi --release
//   # 2) 跑这个脚本（路径指向刚编出的 .so）
//   cd app && dart run tool/ffi_smoke.dart ../core/target/release/libperch_ffi.so
//
// 这是没有 Mac / 没有设备时唯一能**真正**验证 FFI 边界的手段：
// 它走的是和 iOS/Android 完全相同的 C ABI 与 JSON 契约，只是传输层换了加载方式。
//
// 任何一项不符预期就以非零码退出，可直接放进 CI。

import 'dart:convert';
import 'dart:io';

import 'package:perch_core/perch_core.dart';

import 'fixtures.dart';

int _passed = 0;
int _failed = 0;

void check(String name, Object? actual, Object? expected) {
  final ok = '$actual' == '$expected';
  if (ok) {
    _passed++;
    stdout.writeln('  ✓ $name');
  } else {
    _failed++;
    stdout.writeln('  ✗ $name  期望=<$expected> 实际=<$actual>');
  }
}

void checkTrue(String name, bool condition, [String? detail]) {
  if (condition) {
    _passed++;
    stdout.writeln('  ✓ $name');
  } else {
    _failed++;
    stdout.writeln('  ✗ $name${detail == null ? '' : '  $detail'}');
  }
}

Future<void> main(List<String> args) async {
  final libraryPath = args.isNotEmpty
      ? args.first
      : '../core/target/release/libperch_ffi.so';

  if (!File(libraryPath).existsSync()) {
    stderr.writeln('找不到动态库：$libraryPath');
    stderr.writeln('请先运行：cd core && cargo build -p perch-ffi --release');
    exit(2);
  }

  stdout.writeln('加载核心：$libraryPath');
  final core = PerchCore.open(libraryPath: libraryPath);
  stdout.writeln('');

  // ---- 1) 基本连通 ----
  stdout.writeln('[1] 连通性');
  final version = core.version();
  checkTrue('version 返回非空', version.isNotEmpty, 'version=$version');

  // ---- 2) 错误必须作为数据返回，不能崩 ----
  stdout.writeln('[2] 错误处理');
  try {
    core.invoke('不存在的方发');
    checkTrue('未知方法应抛异常', false);
  } on PerchCoreException catch (error) {
    check('未知方法的错误码', error.code, 'FFI_UNKNOWN_METHOD');
  }
  try {
    core.invoke('fingerprint', {'size': 'not-a-number'});
    checkTrue('非法参数应抛异常', false);
  } on PerchCoreException catch (error) {
    check('非法参数的错误码', error.code, 'FFI_BAD_ARGS');
  }

  // ---- 3) 模型层：Locator 校验 ----
  stdout.writeln('[3] Locator 校验');
  core.validateLocator({
    'href': '/OEBPS/c1.xhtml',
    'type': 'application/xhtml+xml',
    // partialCfi 按 Readium 契约是 `!` 右侧的片段，不含 spine 引用与 epubcfi() 包装
    // （见 docs/09 §C.2）。写成 '/6/4!/4/2' 会被当非法值。
    'locations': {'progression': 0.42, 'partialCfi': '/4/2'},
  });
  checkTrue('合法 Locator 通过校验', true);

  try {
    core.validateLocator({'href': '', 'locations': {}});
    checkTrue('空 href 应被拒绝', false);
  } on PerchCoreException catch (error) {
    check('空 href 的错误码', error.code, 'MODEL_EMPTY_FIELD');
  }

  // ---- 4) 导入层：指纹 ----
  stdout.writeln('[4] 文件指纹');
  final first = core.fingerprint(size: 1024, head: [1, 2, 3], tail: [4, 5, 6]);
  final second = core.fingerprint(size: 1024, head: [1, 2, 3], tail: [4, 5, 6]);
  check('指纹稳定可复现', first, second);
  check('指纹长度（64 个十六进制字符）', first.length, 64);
  final different =
      core.fingerprint(size: 1025, head: [1, 2, 3], tail: [4, 5, 6]);
  checkTrue('大小不同 → 指纹不同', different != first);

  // ---- 5) TXT 层：编码探测 ----
  stdout.writeln('[5] 编码探测');
  final utf8Result = core.analyzeText([0xe4, 0xb8, 0xad, 0xe6, 0x96, 0x87]);
  check('UTF-8 解码结果', utf8Result['text'], '中文');
  check('UTF-8 字符数', utf8Result['chars'], 2);
  check('UTF-8 编码名', utf8Result['encoding'], 'UTF-8');

  final gbkResult = core.analyzeText([0xd6, 0xd0, 0xce, 0xc4]);
  check('GBK 解码结果', gbkResult['text'], '中文');
  check('GBK 是猜出来的（不是确定）', gbkResult['certain_encoding'], false);

  // ---- 6) 索引层：检索 ----
  stdout.writeln('[6] 全文检索');
  const text = '前面的内容。中文测试在这里。后面的内容。';
  final hits = core.search(text, '中文测试');
  check('命中数量', hits.length, 1);
  check('命中起点', hits.first['char_start'], 6);
  check('命中终点', hits.first['char_end'], 10);
  checkTrue(
    'snippet 含关键字',
    (hits.first['snippet'] as String).contains('中文测试'),
  );

  final miss = core.search(text, '不存在的词');
  check('查不到时返回空', miss.length, 0);

  // ---- 7) 卷页几何 ----
  stdout.writeln('[7] 卷页几何');
  final noDrag = core.invoke('curl', {
    'page_w': 400,
    'page_h': 700,
    'corner': 'bottom_right',
    'drag_x': 400,
    'drag_y': 700,
    'radius': 20,
  });
  check('未拖动时无几何', noDrag['geometry'], null);

  final dragged = core.invoke('curl', {
    'page_w': 400,
    'page_h': 700,
    'corner': 'bottom_right',
    'drag_x': 200,
    'drag_y': 400,
    'radius': 24,
  });
  final geometry = dragged['geometry'] as Map<String, dynamic>;
  checkTrue(
    '卷角大于 0',
    (geometry['curl_angle'] as num) > 0,
    'curl_angle=${geometry['curl_angle']}',
  );
  checkTrue(
    '正面多边形至少 3 个顶点',
    (geometry['front_outline'] as List).length >= 3,
  );

  // ---- 8) 同步层：HLC ----
  stdout.writeln('[8] 混合逻辑时钟');
  final hlc = core.invoke('hlc.parse', {'hlc': '1760600000000-3-phoneA'});
  check('HLC 物理时间', hlc['wall_ms'], 1760600000000);
  check('HLC 计数', hlc['counter'], 3);
  check('HLC 设备', hlc['device'], 'phoneA');

  // ---- 9) 同步传输层：变更 / 增量游标 / 冲突率 ----
  stdout.writeln('[9] 同步传输层');
  const bookId = '11111111-1111-4111-8111-111111111111';
  Map<String, dynamic> progressChange(String hlc, num progression) => {
        'book': bookId,
        'hlc': hlc,
        'op': 'set',
        'value': {
          'kind': 'progress',
          'locator': {
            'href': 'c1.xhtml',
            'locations': {'progression': progression},
          },
        },
      };

  // 空本地 + 一条远端 → 首次采纳
  final merged = core.mergeSync(
    local: <Map<String, dynamic>>[],
    remote: [progressChange('100-0-phone', 0.5)],
  );
  check('合并后寄存器数', (merged['state'] as List).length, 1);
  check('首次采纳的效果', (merged['effects'] as List).first, 'initialized');
  check('无冲突时冲突率', merged['conflictRate'], 0.0);

  // 两端各自先改，再互相交换 → 状态必须一致
  final phoneSide = core.mergeSync(
    local: [progressChange('100-0-phone', 0.3)],
    remote: [progressChange('110-0-pad', 0.7)],
  );
  final padSide = core.mergeSync(
    local: [progressChange('110-0-pad', 0.7)],
    remote: [progressChange('100-0-phone', 0.3)],
  );
  checkTrue(
    '两设备收敛到同一状态',
    jsonEncode(phoneSide['state']) == jsonEncode(padSide['state']),
    'phone=${phoneSide['state']} pad=${padSide['state']}',
  );

  // 同一 HLC、不同值 → 真正的冲突，应当被计入冲突率
  final conflicted = core.mergeSync(
    local: [progressChange('100-0-phone', 0.5)],
    remote: [progressChange('100-0-phone', 0.9)],
  );
  check('冲突的效果', (conflicted['effects'] as List).first, 'conflictResolved');
  checkTrue(
    '冲突率被计入',
    (conflicted['conflictRate'] as num) > 0,
    'conflictRate=${conflicted['conflictRate']}',
  );

  // 增量游标：拿到一页后带游标再问，应当没有增量
  final page1 = core.deltaSync(state: [progressChange('100-0-phone', 0.5)]);
  check('增量条数', (page1['changes'] as List).length, 1);
  check('游标', page1['nextCursor'], '100-0-phone');
  check('是否还有下一页', page1['hasMore'], false);

  final emptyDelta = core.deltaSync(
    state: [progressChange('100-0-phone', 0.5)],
    since: '100-0-phone',
  );
  check('游标之后无增量', (emptyDelta['changes'] as List).length, 0);

  // ---- 10) PDF 结构 ----
  stdout.writeln('[10] PDF 结构解析');
  // 一份 441 字节的最小 PDF（1 页，Title=Sample），由 lopdf 生成后固化在此。
  // 固化的好处：测试不依赖「生成器」与「解析器」同源，避免互相掩盖错误。
  final pdf = core.analyzePdf(hexBytes(samplePdfHex));
  check('PDF 页数', pdf['pageCount'], 1);
  check('PDF 版本', pdf['version'], '1.7');
  check('PDF 标题', pdf['title'], 'Sample');
  checkTrue(
    '没有目录时 outline 为空',
    ((pdf['outline'] as List?) ?? const []).isEmpty,
    'outline=${pdf['outline']}',
  );

  // 非 PDF 必须明确拒绝，而不是当成空文档
  try {
    core.analyzePdf(utf8.encode('not a pdf'));
    checkTrue('非 PDF 应当报错', false, '没有抛异常');
  } on PerchCoreException catch (error) {
    check('非 PDF 的错误码', error.code, 'PDF_NOT_PDF');
  }

  // ---- 11) 阶段②精修（按错误路径验证边界；成功路径由 Rust FFI 测试覆盖）----
  stdout.writeln('[11] 阶段②精修');
  try {
    core.refineBook(utf8.encode('这不是 epub'), format: 'epub');
    checkTrue('非 EPUB 应当报错', false, '没有抛异常');
  } on PerchCoreException catch (error) {
    check('显式声明 epub 但内容不是 → 解析失败', error.code, 'JOBS_EPUB');
  }

  // ---- 12) 句柄表：打开一本书 → 搜索 → 关闭 ----
  stdout.writeln('[12] 句柄表（有状态索引）');
  final opened = core.openBook(hexBytes(sampleEpubHex));
  final handle = opened['handle'] as int;
  checkTrue('拿到句柄', handle >= 0, 'handle=$handle');
  check('章节目录数', opened['chapterCount'], 1);
  checkTrue(
    '正文字符数大于 0',
    (opened['charCount'] as num) > 0,
    'charCount=${opened['charCount']}',
  );

  // 搜索命中要能还原成指向正确章节的 Locator
  final found = core.searchBook(handle: handle, query: '春天的花开');
  final bookHits = found['hits'] as List;
  check('搜索命中数', bookHits.length, 1);
  final locator = (bookHits.first as Map)['locator'] as Map;
  check('命中所在章节', locator['href'], '/OEBPS/chap0.xhtml');
  check('Locator 带 epub 格式', (locator['perch'] as Map)['format'], 'epub');

  // 关闭之后旧句柄必须被拒绝，而不是误操作到别的书上
  final closed = core.closeBook(handle);
  check('关闭成功', closed['closed'], true);
  try {
    core.searchBook(handle: handle, query: '春天');
    checkTrue('关闭后搜索应当报错', false, '没有抛异常');
  } on PerchCoreException catch (error) {
    check('关闭后的错误码', error.code, 'BOOK_BAD_HANDLE');
  }

  // ---- 13) TXT 全链路：精修 → 打开 → 搜索 → 关闭 ----
  stdout.writeln('[13] TXT 全链路');
  final txtBytes = utf8.encode('第一章\r\n\r\n\r\n苹果和香蕉都很好吃');

  final txtRefined = core.refineBook(txtBytes, format: 'txt');
  checkTrue(
    'TXT 精修给出了编码探测结果',
    txtRefined['encoding'] is String,
    'encoding=${txtRefined['encoding']}',
  );
  checkTrue(
    'TXT 字数大于 0',
    (txtRefined['charCount'] as num) > 0,
    'charCount=${txtRefined['charCount']}',
  );
  checkTrue('TXT 没有内嵌标题', txtRefined['title'] == null, 'title=${txtRefined['title']}');

  final txtOpened = core.openBook(txtBytes, format: 'txt');
  final txtHandle = txtOpened['handle'] as int;
  final txtFound = core.searchBook(handle: txtHandle, query: '苹果');
  final txtHits = txtFound['hits'] as List;
  check('TXT 搜索命中数', txtHits.length, 1);
  final txtLocator = (txtHits.first as Map)['locator'] as Map;
  check('TXT Locator 格式', (txtLocator['perch'] as Map)['format'], 'txt');
  checkTrue(
    'TXT Locator 带字符偏移（跨端最稳的锚点）',
    (txtLocator['perch'] as Map)['txtOffset'] is int,
    'perch=${txtLocator['perch']}',
  );
  core.closeBook(txtHandle);

  // 不给 format 时按内容嗅探，也应当认出这是纯文本
  final sniffed = core.refineBook(utf8.encode('纯文本'));
  checkTrue('嗅探认出了纯文本', (sniffed['charCount'] as num) > 0, 'out=$sniffed');

  // ---- 15) 后台 isolate（UI 不被堵住的前提）----
  //
  // 同步 FFI 直接放在 build/initState 里会堵死 UI isolate（10MB 解码是几百毫秒）。
  // 这里验证：worker 能起来、能调用、错误码跨 isolate 不丢、dispose 后明确失败。
  stdout.writeln('[15] 后台 isolate（PerchWorker）');

  final worker = await PerchWorker.spawn(libraryPath: libraryPath);
  final workerVersion = await worker.call('version');
  check('worker 里能取到核心版本（与主 isolate 一致）', workerVersion['core'], version);

  final workerDir = Directory.systemTemp.createTempSync('perch-worker-');
  final workerFile = File('${workerDir.path}/w.txt');
  workerFile.writeAsStringSync('第一章 灯塔\n守夜人。', encoding: utf8);

  final workerInfo = await worker.call('txt.open', {'path': workerFile.path});
  final workerHandle = workerInfo['handle'] as int;
  check('worker 里打开的 TXT 字符数', workerInfo['chars'], 11);
  final workerSlice = await worker.call('txt.slice', {
    'handle': workerHandle,
    'start': 0,
    'chars': 3,
  });
  check('worker 里的切片', workerSlice['text'], '第一章');

  // 业务错误跨 isolate 之后仍是同一个类型与错误码
  try {
    await worker.call('txt.slice', {'handle': workerHandle, 'chars': 0});
    checkTrue('worker 里的错误应当抛出', false, '没有抛异常');
  } on PerchCoreException catch (error) {
    check('错误码跨 isolate 不丢', error.code, 'TXT_SLICE_BAD_SIZE');
  }

  await worker.call('txt.close', {'handle': workerHandle});
  await worker.dispose();
  try {
    await worker.call('version');
    checkTrue('dispose 之后调用应当抛 StateError', false, '没有抛异常');
  } on StateError {
    checkTrue('dispose 之后调用 → StateError', true);
  }
  workerDir.deleteSync(recursive: true);


  // ---- 14) TXT 阅读路径（句柄化）----
  //
  // 与 [13] 的区别：[13] 走的是 import 阶段的 `txt.analyze`（全文进 JSON），
  // 这里走的是阅读路径 —— 打开一次，之后每次只取一屏。10MB 的书就看这条。
  stdout.writeln('[14] TXT 阅读路径（txt.open / slice / locate / offset / line / close）');

  final readDir = Directory.systemTemp.createTempSync('perch-txt-read-');
  final readFile = File('${readDir.path}/read.txt');
  const readText = '第一章 灯塔\n\n守夜人每天爬上灯塔。\n第二行在这里。';
  readFile.writeAsStringSync(readText, encoding: utf8);

  final reader = core.openTxt(readFile.path);
  check('编码探测', reader.encoding, 'UTF-8');
  check('编码是否确定', reader.certain, true);
  check('字符数（不是字节数）', reader.chars, readText.runes.length);
  check('行数', reader.lines, readText.split('\n').length);

  final slice = reader.slice(3, 5);
  check('切片文本', slice.text, String.fromCharCodes(readText.runes.skip(3).take(5)));
  check('切片起点', slice.start, 3);
  check('切片末尾偏移', slice.end, 8);
  check('全书总字符数', slice.total, readText.runes.length);

  // 变量名别用 locator：上面 [12] 段已经有一个同名的了（同一作用域）
  final readLocator = reader.locate(5);
  check('Locator 带 txtOffset', (readLocator['perch'] as Map)['txtOffset'], 5);
  check('Locator 能还原字符偏移', reader.offsetOf(readLocator), 5);
  check('行号', reader.lineAt(5).line, 0);
  checkTrue('行区间是个有效区间', (reader.lineAt(5).end ?? 0) > (reader.lineAt(5).start ?? -1));

  // 类型不能混用：拿 txt 句柄去 book.close 必须被拒绝，**且不能真把它关掉**
  try {
    core.closeBook(reader.handle);
    checkTrue('txt 句柄调 book.close 应当报错', false, '没有抛异常');
  } on PerchCoreException catch (error) {
    check('类型不符的错误码', error.code, 'HANDLE_WRONG_KIND');
  }
  check('被拒绝后句柄仍然可用', reader.slice(0, 1).text, '第');

  // 配额：TXT 与"书索引"**分开计数**，上限 4 个
  final handles = <TxtHandle>[reader];
  for (var i = 0; i < 3; i++) {
    handles.add(core.openTxt(readFile.path));
  }
  try {
    core.openTxt(readFile.path);
    checkTrue('超过 TXT 配额应当报错', false, '没有抛异常');
  } on PerchCoreException catch (error) {
    check('TXT 配额错误码', error.code, 'TXT_TOO_MANY_OPEN');
  }

  // 关掉一个就腾出名额，且**新句柄不与旧的复用**（复用会让旧引用误操作到新对象）
  final freed = handles.removeLast();
  freed.close();
  final reopened = core.openTxt(readFile.path);
  checkTrue('关掉一个后能再开', reopened.handle != freed.handle,
      '复用了旧句柄 ${reopened.handle}');
  handles.add(reopened);

  // 取整本书必须明确失败，而不是悄悄传一个巨大的字符串
  try {
    reader.slice(0, 1000000);
    checkTrue('一次要一百万个字符应当报错', false, '没有抛异常');
  } on PerchCoreException catch (error) {
    check('切片过大的错误码', error.code, 'TXT_SLICE_BAD_SIZE');
  }

  // Dart 侧的兜底：close 之后再调，给的是可读的 StateError，而不是核心的 BAD_HANDLE
  reader.close();
  try {
    reader.slice(0, 1);
    checkTrue('close 之后再切片应当抛 StateError', false, '没有抛异常');
  } on StateError {
    checkTrue('close 之后再切片 → StateError', true);
  }
  for (final handle in handles) {
    handle.close();
  }
  readDir.deleteSync(recursive: true);

  // ---- 汇总 ----
  stdout.writeln('');
  stdout.writeln('通过 $_passed 项，失败 $_failed 项');
  if (_failed > 0) {
    exit(1);
  }
  stdout.writeln('FFI 边界端到端验证通过。');
}
