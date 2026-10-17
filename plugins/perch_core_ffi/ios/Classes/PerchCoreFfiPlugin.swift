import Flutter

/// 本插件**不提供任何 MethodChannel**。
///
/// 它存在的目的有两个：
/// 1. 让 Flutter 的 CocoaPods 集成把 `Podfile` 指向我们的 podspec，从而执行
///    `Build Rust core` 构建阶段，把 Rust 核心编译成 iOS 静态库；
/// 2. 通过 `perch_link_anchor()`（见 perch_link_anchor.c）建立对
///    `perch_call` / `perch_string_free` 的链接期引用，防止链接器把
///    Rust 静态库里的内容当死代码丢掉。
///
/// 之后 Dart 侧用 `DynamicLibrary.process()` 就能解析到这两个符号
/// （见 dart/perch_core/lib/perch_core.dart）。
@_silgen_name("perch_link_anchor")
private func perch_link_anchor()

public class PerchCoreFfiPlugin: NSObject, FlutterPlugin {
  public static func register(with registrar: FlutterPluginRegistrar) {
    // 显式调用锚点：保证它不被优化掉，从而保证 Rust 符号真的进了二进制。
    perch_link_anchor()
  }
}
