# Spike 报告：flutter_readium 可行性验证

> 目的：为 `docs/06-UI技术栈决策.md` 的 go/no-go 提供实测依据。
> 结论时间：2026-10-16。**部分通过 —— 编译与集成成立；web 运行时路径有两个坑未全部解决；真机路径待验证。**

## 一、结论速览

| 验证项 | 结果 |
|---|---|
| `flutter_readium 0.6.2` 能否装进工程 | ✅ 走 TUNA 镜像，25 个依赖正常解析 |
| 能否编译为 web release | ✅ `flutter build web --release` 17~18 秒通过 |
| 读到 Readium 的 `Locator` 结构 | ✅ **与我们的 Rust 设计逐字段一致**（见 §四，本次最大收获） |
| 在浏览器里真的打开一本 EPUB | ❌ **未成功**（两个坑：见 §三；缺 JS 已修，manifest.json 问题未解） |
| 在 Android / iOS 上打开 | ⏳ 未验证（需要系统镜像或云端 macOS CI） |

**判定**：web 是 `flutter_readium` 里最不被照顾的路径（且我们也不发布 web）。**要判定 Flutter 路线是否成立，必须用 Android 或 iOS 真机路径**——那才是我们要发布的、由官方 Readium Kotlin/Swift 工具链驱动的代码路径。

## 二、本次验证到的"必须做"的配置（README 里不显眼，漏了就失败）

### 2.1 Web：必须手工注入 Readium JS

```bash
dart run flutter_readium:copy_js_file web      # 产出 web/readiumReader.js（671 KB）
```
并在 `web/index.html` 引用：
```html
<script src="readiumReader.js" defer></script>
```

**漏掉这一步的报错是**：
```
TypeError: dart.global.ReadiumReader is not a constructor
    at get _readiumReader (js_publication_channel.dart:113)
```
插件**不自己打包**这份 JS，而是要求宿主页面提供全局 `ReadiumReader`。
→ 这条已写入本仓库（`app/web/`），后续不会再踩。

### 2.2 Web：publication URL 必须是**绝对地址**

相对路径会报：
```
ReadiumException{code: unknown, message: Failed to construct 'URL': Invalid base URL}
```
因为 Readium 内部用 `new URL(...)` 解析。正确写法：
```dart
final url = Uri.base.resolve('assets/$_assetKey').toString();  // → http://host/assets/...
```

### 2.3 其他平台要求（来自 README，尚未验证）

- Android：`minSdkVersion >= 24`、启用 core library desugaring、`MainActivity` 必须继承 `FlutterFragmentActivity`
- iOS：15.0+；macOS 原生**不支持**（上游 swift-toolkit 标了 not_planned）
- 版本要求：Flutter ≥ 3.44.8（本机 3.47.7 ✓）、Dart ≥ 3.8（本机 3.13.5 ✓）

## 三、未解决的问题（web 路径）

修掉上述两点后，`openPublication` 走得更远，但报出：

```
ReadiumException{code: notFound, message: http GET request for
  http://localhost:8099/assets/assets/manifest.json failed with HTTP status code 404}
```

**Readium 把同目录下的 `manifest.json` 当成了出版物入口**，即把 URL 当成"WebPub 目录式出版物"而不是 EPUB 文件。

已排除的可能原因：
- ❌ **不是** MIME 问题：实测 Python `http.server` 对 `.epub` 已经返回 `Content-type: application/epub+zip`
- ❌ **不是** 文件坏了：同一个 EPUB 结构自检通过（mimetype 首个条目 + Stored）

**下一步要查**：`flutter_readium_web.dart` 的 `loadPublication` 如何判定"EPUB 文件 vs WebPub manifest"——怀疑 web 实现只支持传 **manifest.json 的 URL**（WebPub 形态），而不支持直接传 `.epub` 文件 URL。若如此，这是一个**应向上游报的 issue**（或需要 fork 修）。

## 四、本次最重要的收获：Locator 设计与 Readium 完全对上了

这是花时间做 spike 的最大回报 —— **`perch-model::Locator` 不是猜的，是被实测确认的**。

读 `flutter_readium_platform_interface` 源码确认：

```dart
class Locator extends AdditionalProperties implements JSONable {
  final String href;          // 与我们的 href 一致
  final String type;          // 与我们的 media_type（JSON 键名 "type"）一致
  final String? title;        // 一致
  final Locations? locations; // 一致
  final LocatorText? text;    // 一致
}
```

| Readium | 我们的 `perch-model` | 结论 |
|---|---|---|
| `Locator.href` | `Locator.href` | ✅ 同名同义 |
| `Locator.type` | `Locator.media_type`（serde rename 为 `"type"`） | ✅ JSON 上完全一致 |
| `Locations.fragments` | `Locations.fragments` | ✅ |
| `Locations.progression` | `Locations.progression` | ✅ |
| `Locations.totalProgression` | `Locations.total_progression` | ✅ |
| `Locations.position` | `Locations.position` | ✅ |
| `Locations.cssSelector` | `Locations.css_selector` | ✅ |
| `Locations.partialCfi` | `Locations.partial_cfi` | ✅（注意是 `partialCfi`，不是 `cfi`——当初就是这么设计的） |
| `LocatorText.before/highlight/after` | `TextContext.before/highlight/after` | ✅ |
| `extends AdditionalProperties` | `perch` 扩展对象 | ✅ **未知字段会被 Readium 保留**，我们的扩展即使过一遍 Readium 也不会丢 |

**推论**：两边互转可以退化为**字段拷贝**，不需要任何映射表或适配层。当初把扩展放进独立的 `perch` 对象、并且严格照 Readium 命名（`partialCfi`）的两个决定，现在被证明是对的。

## 五、建议的下一步（按价值排序）

1. **用 Android 路径验证**（最接近发布形态）：
   - 选项 A：`waydroid init` 下载 Android 镜像（1~2 GB，本机 Waydroid 1.6.3 已装、容器服务已 active，且 `/dev/kvm` 可用）
   - 选项 B：`sdkmanager --install "emulator" "system-images;android-36;google_apis;x86_64"`（约 2 GB，`dl.google.com` 已实测可达）
   - 这一步能回答真正的问题：Android 上（Readium Kotlin）能否渲染中文、拿到 Locator、做高亮
2. **同时**把 iOS 侧接到云端 macOS CI（`macos-26-arm64` + Xcode 26.6），先只做"能编译 + 能跑模拟器测试"
3. web 的 `manifest.json` 问题可以**暂时不管**（我们不发布 web），但如果要向上游报 issue，本次的最小复现已经很清楚

## 六、附：本次产出的可复用资产

| 文件 | 说明 |
|---|---|
| `tools/make_spike_epub.py` | 生成中文测试 EPUB（3 章、含 CSS、EPUB3 NAV），**无版权问题**，可复现 |
| `app/assets/spike_book.epub` | 上面脚本生成的产物（4190 字节） |
| `app/lib/main.dart` | spike 界面：`ReadiumReaderWidget` + Locator 原始 JSON 面板 + 翻页按钮；日志同时 `debugPrint` 到控制台，**便于 CI 用无头浏览器抓取验证结果** |
| `app/web/readiumReader.js` | `copy_js_file` 的产物（671 KB），web 运行必需 |

**验证手法值得保留**：`chromium --headless --enable-logging=stderr --dump-dom` + 在 Dart 里 `debugPrint`，
就能在**没有图形界面、也没有 Mac** 的 Linux 上，把 Flutter 应用的运行时行为变成可断言的文本输出。
这套手法同样适用于接入 CI。
