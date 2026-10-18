// 核心自检的单测。
//
// 为什么值得单独测：CI 的模拟器冒烟**断言的就是这里产出的那行日志**。
// 日志格式一旦漂移，CI 会红，而原因看起来会像"App 起不来"——
// 所以格式本身要有测试兜住（`logLine` 的断言），两条失败路径也要覆盖。
//
// 用假 [CoreProbe]，不加载 .so：这样纯 Dart VM 就能跑。

import 'package:flutter_test/flutter_test.dart';
import 'package:perch/core_self_check.dart';

/// 带稳定错误码的假异常（模拟 `PerchCoreException` 的鸭子类型）。
class _FakeCoreException implements Exception {
  _FakeCoreException(this.code, this.message);

  final String code;
  final String message;

  @override
  String toString() => 'FakeCoreException($code): $message';
}

/// 可编程的假核心。
class _FakeProbe implements CoreProbe {
  _FakeProbe({this.versionResult = '0.1.0', this.versionError, this.validateError});

  final String versionResult;
  final Object? versionError;
  final Object? validateError;

  /// 记录收到的 Locator，用来断言"自检真的传了参数"。
  Map<String, dynamic>? received;

  @override
  String version() {
    final error = versionError;
    if (error != null) {
      throw error;
    }
    return versionResult;
  }

  @override
  void validateLocator(Map<String, dynamic> locator) {
    received = locator;
    final error = validateError;
    if (error != null) {
      throw error;
    }
  }
}

void main() {
  group('runSelfCheck', () {
    test('核心正常时通过，并带上版本号', () {
      final result = runSelfCheck(_FakeProbe(versionResult: '0.1.0'));

      expect(result.ok, isTrue);
      expect(result.coreVersion, '0.1.0');
      expect(result.errorCode, isNull);
    });

    test('真的调用了业务方法，而不是只看版本号', () {
      final probe = _FakeProbe();
      runSelfCheck(probe);

      expect(probe.received, isNotNull);
      expect(probe.received!['href'], isA<String>());
      expect(probe.received!['locations'], isA<Map<String, dynamic>>());
    });

    test('version 失败时降级，并透传核心的错误码', () {
      final result = runSelfCheck(
        _FakeProbe(versionError: _FakeCoreException('FFI_NULL', '返回了空指针')),
      );

      expect(result.ok, isFalse);
      expect(result.errorCode, 'FFI_NULL');
      expect(result.coreVersion, isNull);
    });

    test('locator.validate 失败时降级，并透传核心的错误码', () {
      final result = runSelfCheck(
        _FakeProbe(validateError: _FakeCoreException('MODEL_EMPTY_FIELD', 'href 为空')),
      );

      expect(result.ok, isFalse);
      expect(result.errorCode, 'MODEL_EMPTY_FIELD');
    });

    test('没有 code 字段的异常退回本文件的错误码', () {
      final versionFailure = runSelfCheck(
        _FakeProbe(versionError: StateError('核心炸了')),
      );
      expect(versionFailure.errorCode, 'SELFCHECK_VERSION_FAILED');

      final validateFailure = runSelfCheck(
        _FakeProbe(validateError: StateError('核心炸了')),
      );
      expect(validateFailure.errorCode, 'SELFCHECK_VALIDATE_FAILED');
    });

    test('连 Error（不是 Exception）也不外泄', () {
      final result = runSelfCheck(_FakeProbe(versionError: ArgumentError('没有符号')));

      expect(result.ok, isFalse);
      expect(result.errorCode, 'SELFCHECK_VERSION_FAILED');
      expect(result.errorMessage, contains('没有符号'));
    });
  });

  group('日志格式（CI 断言的契约）', () {
    test('成功行的格式是 [perch-selfcheck] ok core=<版本>', () {
      final result = runSelfCheck(_FakeProbe(versionResult: '9.9.9'));

      expect(result.logLine, '$kSelfCheckLogTag ok core=9.9.9');
      expect(result.logLine, '[perch-selfcheck] ok core=9.9.9');
    });

    test('失败行带着错误码与原因，方便在 CI 日志里定位', () {
      final result = runSelfCheck(
        _FakeProbe(versionError: _FakeCoreException('SELFCHECK_OPEN_FAILED', '找不到符号')),
      );

      expect(result.logLine, contains('$kSelfCheckLogTag failed'));
      expect(result.logLine, contains('code=SELFCHECK_OPEN_FAILED'));
      expect(result.logLine, contains('找不到符号'));
    });

    test('角标文案短，且能区分成功与失败', () {
      expect(runSelfCheck(_FakeProbe(versionResult: '0.1.0')).badgeLabel, 'core 0.1.0');
      expect(
        runSelfCheck(_FakeProbe(versionError: StateError('x'))).badgeLabel,
        'core 未连接',
      );
    });
  });
}
