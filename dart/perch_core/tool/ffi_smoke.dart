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

import 'dart:io';

import 'package:perch_core/perch_core.dart';

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

void main(List<String> args) {
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

  // ---- 汇总 ----
  stdout.writeln('');
  stdout.writeln('通过 $_passed 项，失败 $_failed 项');
  if (_failed > 0) {
    exit(1);
  }
  stdout.writeln('FFI 边界端到端验证通过。');
}
