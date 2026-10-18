// web 上的自检实现：`dart:ffi` 在 web 不存在，所以这里只报告「本平台不支持」。
//
// 存在的意义是让 `flutter build web` 仍然能过——这个 stub 不是"失败"，
// 而是"这个平台上本来就没有 FFI 这一层"。

import 'core_self_check.dart';

/// web：没有 FFI。
CoreSelfCheck selfCheckCore() => const CoreSelfCheck.failed(
  code: 'SELFCHECK_UNSUPPORTED',
  message: '当前平台没有 ffi 实现（web）',
);
