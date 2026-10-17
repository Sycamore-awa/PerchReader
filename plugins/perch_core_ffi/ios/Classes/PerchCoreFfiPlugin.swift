import Flutter

/// 本插件**不提供任何 MethodChannel**。
///
/// 它存在的唯一目的：让 Flutter 的 CocoaPods 集成把
/// `Podfile` 指向我们的 podspec，从而执行 `Build Rust core` 构建阶段，
/// 并用 `-force_load` 把 `libperch_ffi.a` 链进最终二进制。
///
/// 之后 Dart 侧用 `DynamicLibrary.process()` 就能解析到
/// `_perch_call` / `_perch_string_free`（见 dart/perch_core/lib/perch_core.dart）。
public class PerchCoreFfiPlugin: NSObject, FlutterPlugin {
  public static func register(with registrar: FlutterPluginRegistrar) {
    // 无需注册任何 channel。
  }
}
