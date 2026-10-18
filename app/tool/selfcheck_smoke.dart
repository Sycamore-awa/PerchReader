// 在 Linux 上**真的**跑一遍「App 启动自检」的逻辑。
//
// 为什么值得有：自检是"App 运行时真的能用上 Rust 核心"的证据（Step 1a），
// 但它在 iOS 上的执行要等 CI 的模拟器冒烟。这个工具把**同一份代码**
// 在 Linux 上跑到真实的 `libperch_ffi.so` 上——不需要设备、不需要 Mac。
//
// 用法：
//   dart run tool/selfcheck_smoke.dart [libperch_ffi.so 的路径]
//   （不带参数时用 ../core/target/release/libperch_ffi.so，即 just core-lib 的产物）
//
// ignore_for_file: avoid_print —— 这是命令行工具，print 就是它的输出通道。
//
// 覆盖两条路径：
//   ① 核心可用   → 自检必须 ok，且日志行格式正确
//   ② 核心不可用 → 自检必须**优雅降级**（返回失败结果，而不是抛异常）

import 'package:perch/core_self_check.dart';
import 'package:perch/core_self_check_io.dart' as native;
import 'package:perch_core/perch_core.dart';

int _failed = 0;

void check(String label, bool condition, {String? detail}) {
  if (condition) {
    print('  ✓ $label');
  } else {
    _failed++;
    print('  ✗ $label${detail == null ? '' : ' —— $detail'}');
  }
}

void _finish() {
  if (_failed == 0) {
    print('\n全部通过：自检逻辑在 Linux 上对真实核心可用。');
  } else {
    print('\n失败 $_failed 项');
  }
}

void main(List<String> args) {
  final libraryPath = args.isEmpty
      ? '../core/target/release/libperch_ffi.so'
      : args.first;

  print('[1] 核心可用：真实加载 $libraryPath');
  final CoreSelfCheck okResult;
  try {
    okResult = native.selfCheckCore(PerchCore.open(libraryPath: libraryPath));
  } on Object catch (error) {
    _failed++;
    print('  ✗ 自检把异常抛出来了（违反"永不抛异常"）：$error');
    _finish();
    return;
  }
  print('    日志行：${okResult.logLine}');
  check('自检通过', okResult.ok, detail: '错误码 ${okResult.errorCode}');
  check('拿到了核心版本', okResult.coreVersion != null);
  check(
    '日志行格式稳定（CI 靠它断言）',
    okResult.logLine.startsWith('$kSelfCheckLogTag ok core='),
    detail: okResult.logLine,
  );
  check('角标可读', okResult.badgeLabel.startsWith('core '), detail: okResult.badgeLabel);

  print('[2] 核心不可用：走平台默认加载路径时的降级（Linux 上通常找不到 .so）');
  final CoreSelfCheck degraded;
  try {
    // 不带参数 → 走 PerchCore 的平台默认路径，失败发生在 selfCheckCore 内部。
    degraded = selfCheckCore();
  } on Object catch (error) {
    _failed++;
    print('  ✗ 自检把异常抛出来了（违反"永不抛异常"）：$error');
    _finish();
    return;
  }
  print('    日志行：${degraded.logLine}');
  if (degraded.ok) {
    // 只要不抛异常就算过：库里碰巧在加载路径上时，这条路本应是成功的。
    check('默认路径可用（LD_LIBRARY_PATH 里有 libperch_ffi.so）', true);
  } else {
    check('没有抛异常，而是降级为失败结果', true);
    check('带上了可读的错误码', degraded.errorCode != null);
    check(
      '失败行格式稳定',
      degraded.logLine.startsWith('$kSelfCheckLogTag failed code='),
      detail: degraded.logLine,
    );
  }

  _finish();
}
