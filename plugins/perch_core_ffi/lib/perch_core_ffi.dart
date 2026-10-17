/// 占位包：本插件的价值全在**原生构建阶段**（把 Rust 静态库链接进来），
/// 没有 Dart 侧逻辑。
///
/// 真正的 FFI 绑定在 `dart/perch_core/lib/perch_core.dart`。
library;

/// 仅用于让测试/工具能确认插件被正确解析。
const String perchCoreFfiPluginVersion = '0.1.0';
