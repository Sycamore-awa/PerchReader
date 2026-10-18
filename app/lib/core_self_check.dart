// 核心自检：证明「App **运行时**真的能用上 Rust 核心」。
//
// 为什么需要它：`app/lib` 此前从不 import `perch_core`——也就是说 FFI 只在
// CI 的符号断言（`nm` 里找到 `_perch_call`）与 Dart 冒烟里被验证过，而
//
//   「符号在 bundle 里」 ≠ 「DynamicLibrary.process() 运行时解析得到」
//
// 是两件事。这条自检把后者变成**每次启动都会留下证据的一行日志**，CI 的模拟器
// 冒烟就断言这一行（见 `.github/workflows/ci.yml` 的「模拟器冒烟」步骤）。
//
// 三条硬约束：
// 1. **不抛异常**——核心连不上是「降级」，不是「崩溃」（目标：崩溃率 < 0.1%）。
// 2. **本文件不 import `dart:ffi` 也不 import Flutter**——FFI 那部分放在
//    `core_self_check_io.dart`，由条件导入在 web 上换成 stub。
//    原因：`package:perch_core` 依赖 `dart:ffi`，web 上根本编译不过；
//    而 `just app-web` / `app-headless-check` 是本机的预览回路，不能被弄坏。
// 3. **日志格式稳定**——`[perch-selfcheck] ok core=<版本>` 是 CI 断言的契约，
//    改它等于改 CI 断言，两边必须一起改。

import 'core_self_check_stub.dart'
    if (dart.library.io) 'core_self_check_io.dart' as platform;

/// CI 抓取的稳定标记。**改这里就要同步改 CI 的断言。**
const String kSelfCheckLogTag = '[perch-selfcheck]';

/// 自检用的最小核心接口。
///
/// 真实实现是 `PerchCore`（经 `PerchCoreProbe` 适配）；测试里传假实现，
/// 这样「核心正常 / 核心报错」两条路径都能覆盖，且不需要 .so。
abstract interface class CoreProbe {
  /// 读核心版本（对应 FFI `version`）。
  String version();

  /// 校验一个 Locator（对应 FFI `locator.validate`）。
  void validateLocator(Map<String, dynamic> locator);
}

/// 自检结果。**永不抛异常**，失败也是一种结果。
class CoreSelfCheck {
  const CoreSelfCheck._({
    required this.ok,
    this.coreVersion,
    this.errorCode,
    this.errorMessage,
  });

  /// 成功：核心能打开、能返回版本、能跑通一次真实业务调用。
  const CoreSelfCheck.passed(String version)
    : this._(ok: true, coreVersion: version);

  /// 失败：核心打不开或调用报错。UI 应当降级（角标变灰/红 + 日志），而不是崩溃。
  const CoreSelfCheck.failed({required String code, required String message})
    : this._(ok: false, errorCode: code, errorMessage: message);

  /// 核心是否可用。
  final bool ok;

  /// 核心版本（成功时非空）。
  final String? coreVersion;

  /// 失败时的稳定错误码（Rust 侧错误码，或本文件的 `SELFCHECK_*`）。
  final String? errorCode;

  /// 失败时的人类可读信息。
  final String? errorMessage;

  /// 写进日志的那一行。**CI 断言的就是它。**
  String get logLine => ok
      ? '$kSelfCheckLogTag ok core=$coreVersion'
      : '$kSelfCheckLogTag failed code=$errorCode message=$errorMessage';

  /// 角标文案（短，不换行）。
  String get badgeLabel => ok ? 'core $coreVersion' : 'core 未连接';

  /// 角标提示（鼠标长按 / 无障碍朗读用）。
  String get badgeTooltip => ok
      ? 'Rust 核心已连接：$coreVersion'
      : 'Rust 核心不可用：$errorCode — $errorMessage';

  @override
  String toString() => logLine;
}

/// 自检主体（可注入，便于测试）。
///
/// 先要版本号，再走一次**真实业务调用** `locator.validate`：
/// 只读版本号证明不了"参数能过边界"，而 JSON 编解码正是这条契约最容易坏的地方。
CoreSelfCheck runSelfCheck(CoreProbe probe) {
  final String version;
  try {
    version = probe.version();
  } on Object catch (error) {
    return CoreSelfCheck.failed(
      code: _errorCodeOf(error, fallback: 'SELFCHECK_VERSION_FAILED'),
      message: '$error',
    );
  }

  try {
    // 形状取自 core/perch-ffi 的单测：
    // `{"href":"/c1.xhtml","locations":{"progression":0.5}}` 是合法 Locator。
    // progression 取 0.0，因为阅读器首屏就是这个位置。
    probe.validateLocator(const {
      'href': '/selfcheck.xhtml',
      'locations': {'progression': 0.0},
    });
  } on Object catch (error) {
    return CoreSelfCheck.failed(
      code: _errorCodeOf(error, fallback: 'SELFCHECK_VALIDATE_FAILED'),
      message: '$error',
    );
  }

  return CoreSelfCheck.passed(version);
}

/// 打开核心并跑一次自检。
///
/// 打开失败在 iOS 上通常意味着静态库没链接进来（`lookupFunction` 找不到符号），
/// 那正是本项目花了好几轮 CI 才修好的问题——所以这条路径必须留下**可读**的错误。
/// 具体实现见 `core_self_check_io.dart`（web 上是 stub）。
CoreSelfCheck selfCheckCore() => platform.selfCheckCore();

/// 从异常里取稳定错误码。
///
/// 这里刻意不用 `on PerchCoreException`——那个类型来自 `package:perch_core`，
/// 一旦 import 进来，web 构建就会因为 `dart:ffi` 而失败（见文件头第 2 条）。
/// 用鸭子类型取 `code` 字段，等价且不引入依赖。
String _errorCodeOf(Object error, {required String fallback}) {
  try {
    final dynamic dynamicError = error;
    final Object? code = dynamicError.code as Object?;
    if (code is String && code.isNotEmpty) {
      return code;
    }
  } on Object {
    // 取不到就退回本文件的错误码
  }
  return fallback;
}
