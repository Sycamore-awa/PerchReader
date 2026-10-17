// 书库与进度的单元测试。
//
// 用 `flutter test` 跑在纯 Dart VM 上，**不需要模拟器或真机** ——
// 这是没有设备时唯一能真正验证产品逻辑的手段。

import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:perch/library.dart';

void main() {
  late Directory temp;

  setUp(() async {
    temp = await Directory.systemTemp.createTemp('perch_test_');
  });

  tearDown(() async {
    if (await temp.exists()) {
      await temp.delete(recursive: true);
    }
  });

  const book = BookEntry(
    id: 'book-1',
    title: '灯塔守夜人的两页书',
    format: 'epub',
    path: 'assets/spike_book.epub',
  );

  Map<String, dynamic> locator(double progression) => {
    'href': '/OEBPS/text/chap01.xhtml',
    'type': 'application/xhtml+xml',
    'locations': {'progression': progression, 'partialCfi': '/6/4!/4/2'},
  };

  group('BookEntry', () {
    test('JSON 往返不丢字段', () {
      final restored = BookEntry.fromJson(book.toJson());
      expect(restored.id, book.id);
      expect(restored.title, book.title);
      expect(restored.format, book.format);
      expect(restored.path, book.path);
    });
  });

  group('JsonFileProgressStore', () {
    test('没有进度时返回 null', () async {
      final store = JsonFileProgressStore(temp);
      expect(await store.load(book.id), isNull);
    });

    test('写入后能读回同样的内容', () async {
      final store = JsonFileProgressStore(temp);
      await store.save(book.id, locator(0.42));

      final loaded = await store.load(book.id);
      expect(loaded, isNotNull);
      expect(loaded!['href'], '/OEBPS/text/chap01.xhtml');
      final locations = loaded['locations'] as Map<String, dynamic>;
      expect(locations['progression'], 0.42);
      expect(locations['partialCfi'], '/6/4!/4/2');
    });

    test('重复写入是覆盖而不是追加', () async {
      final store = JsonFileProgressStore(temp);
      await store.save(book.id, locator(0.1));
      await store.save(book.id, locator(0.9));

      final loaded = await store.load(book.id);
      final locations = loaded!['locations'] as Map<String, dynamic>;
      expect(locations['progression'], 0.9);
    });

    test('不同书互不影响', () async {
      final store = JsonFileProgressStore(temp);
      await store.save('a', locator(0.1));
      await store.save('b', locator(0.9));

      final first = await store.load('a');
      final second = await store.load('b');
      expect((first!['locations'] as Map)['progression'], 0.1);
      expect((second!['locations'] as Map)['progression'], 0.9);
    });

    test('清除后读不到', () async {
      final store = JsonFileProgressStore(temp);
      await store.save(book.id, locator(0.5));
      await store.clear(book.id);
      expect(await store.load(book.id), isNull);
    });

    test('文件损坏时返回 null 而不是抛异常', () async {
      final store = JsonFileProgressStore(temp);
      await store.save(book.id, locator(0.5));
      // 手工破坏文件内容
      await File('${temp.path}/progress_${book.id}.json').writeAsString('{坏掉的');

      expect(await store.load(book.id), isNull, reason: '坏数据不该让 App 打不开书');
    });

    test('目录不存在时会自动创建', () async {
      final nested = Directory('${temp.path}/deep/nested');
      final store = JsonFileProgressStore(nested);
      await store.save(book.id, locator(0.3));
      expect(await store.load(book.id), isNotNull);
    });
  });

  group('ReadingSession', () {
    test('记录后能恢复同一个位置', () async {
      final session = ReadingSession(store: JsonFileProgressStore(temp));
      await session.record(book, locator(0.66));

      final restored = await session.restore(book);
      expect(restored, isNotNull);
      final locations = restored!['locations'] as Map<String, dynamic>;
      expect(locations['progression'], 0.66);
    });

    test('没有记录时恢复返回 null', () async {
      final session = ReadingSession(store: JsonFileProgressStore(temp));
      expect(await session.restore(book), isNull);
    });

    test('非法 Locator 不写入', () async {
      var calls = 0;
      final session = ReadingSession(
        store: JsonFileProgressStore(temp),
        // 假校验器：只拒绝缺少 href 的
        validator: (value) {
          calls++;
          if ((value['href'] as String?)?.isEmpty ?? true) {
            throw const FormatException('href 为空');
          }
        },
      );

      await expectLater(
        session.record(book, {'href': '', 'locations': {}}),
        throwsA(isA<FormatException>()),
      );
      expect(calls, 1);
      // 关键断言：校验失败后存储里不该留下任何东西
      expect(await session.restore(book), isNull);
    });

    test('恢复时校验失败则当作没有进度', () async {
      final store = JsonFileProgressStore(temp);
      // 直接写一个"坏"的（绕过校验）
      await store.save(book.id, {'href': '', 'locations': {}});

      final session = ReadingSession(
        store: store,
        validator: (value) {
          if ((value['href'] as String?)?.isEmpty ?? true) {
            throw const FormatException('href 为空');
          }
        },
      );

      expect(
        await session.restore(book),
        isNull,
        reason: '坏数据应被忽略，而不是抛给 UI',
      );
    });

    test('没有校验器时也能工作', () async {
      final session = ReadingSession(store: JsonFileProgressStore(temp));
      await session.record(book, locator(0.25));
      expect(await session.restore(book), isNotNull);
    });
  });
}
