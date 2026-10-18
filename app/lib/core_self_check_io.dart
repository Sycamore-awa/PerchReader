// 原生平台的自检实现：真的打开 Rust 核心并调用它。
//
// 为什么单独一个文件：`package:perch_core` 依赖 `dart:ffi`，而 web 上不存在
// `dart:ffi`——如果直接 import 进 `core_self_check.dart`，`flutter build web`
// 会编译失败，把本机唯一的预览回路（`just app-web` / `app-headless-check`）弄坏。
// 条件导入见 `core_self_check.dart` 文件头。

import 'package:perch_core/perch_core.dart';

import 'core_self_check.dart';

/// 把 `PerchCore` 适配成 [CoreProbe]（测试里可以换成假实现）。
class PerchCoreProbe implements CoreProbe {
  PerchCoreProbe(this._core);

  final PerchCore _core;

  @override
  String version() => _core.version();

  @override
  void validateLocator(Map<String, dynamic> locator) =>
      _core.validateLocator(locator);
}

/// 打开核心并自检。供 `core_self_check.dart` 转发（那里的签名不带参数）。
CoreSelfCheck selfCheckCore([PerchCore? core]) {
  final CoreProbe probe;
  try {
    // iOS：符号链进 App，走 DynamicLibrary.process()。
    // 符号缺失时 lookupFunction 抛 ArgumentError —— 正是我们要看见的失败。
    probe = PerchCoreProbe(core ?? PerchCore.open());
  } on Object catch (error) {
    return CoreSelfCheck.failed(
      code: 'SELFCHECK_OPEN_FAILED',
      message: '$error',
    );
  }
  return runSelfCheck(probe);
}
