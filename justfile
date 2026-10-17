# PerchReader 开发任务入口
# 用法：just            → 列出所有任务
#       just ci         → 本地跑一遍 CI 会做的检查

# ---- 默认 ----
default:
    @just --list

# ---- Rust 核心（全部可在 Linux 本地运行） ----

# 格式化
fmt:
    cd core && cargo fmt --all

# 格式检查（CI 用，不修改文件）
fmt-check:
    cd core && cargo fmt --all --check

# 静态检查（unwrap/expect/panic 已在 workspace lints 里禁掉）
lint:
    cd core && cargo clippy --workspace --all-targets -- -D warnings

# 单元测试
test:
    cd core && cargo test --workspace

# ---- iOS 交叉校验（不需要 Mac！） ----

# 校验核心能为 iOS 真机编译
check-ios:
    cd core && cargo check --workspace --target aarch64-apple-ios

# 校验能为两种模拟器架构编译
check-ios-sim:
    cd core && cargo check --workspace --target aarch64-apple-ios-sim
    cd core && cargo check --workspace --target x86_64-apple-ios

# 产出 iOS 静态库（Linux 上可产出 .a，链接成 App 仍需 macOS）
# 编译 iOS 静态库
# 必须用 `cargo rustc --crate-type staticlib` 而不是 `cargo build`：
# perch-ffi 声明了 cdylib + staticlib + rlib，而 cdylib 需要链接器，
# Linux 上的 cc 不认识 `-target arm64-apple-ios` → 整个 build 会报错退出。
# staticlib 只是打包 .a，不需要链接器，因此覆盖 crate-type 即可。
build-ios-lib:
    cd core && cargo rustc -p perch-ffi --release --target aarch64-apple-ios --crate-type staticlib

# 断言 iOS 静态库导出了 FFI 符号（docs/09 §D.5 的零成本检查）
# T = 全局可导出；若变成 t（局部）或 U（未定义），DynamicLibrary.process() 运行时找不到符号
ios-symbols: build-ios-lib
    #!/usr/bin/env bash
    set -euo pipefail
    lib=core/target/aarch64-apple-ios/release/libperch_ffi.a
    nm_bin=$(command -v llvm-nm || command -v nm)
    # 先整体取出符号表再比较：**不能**写成 `nm ... | grep -q`。
    # grep -q 命中后立即退出会关掉管道，nm 收到 SIGPIPE 而非零退出，
    # 在 set -o pipefail 下整个管道被判失败 —— 表现为"匹配成功却报失败"。
    symbols=$("$nm_bin" --defined-only -g "$lib" 2>/dev/null)  # 2>/dev/null: 屏蔽空目标文件的 "no symbols" 噪声
    for symbol in _perch_call _perch_string_free; do
      if ! grep -q "T $symbol$" <<<"$symbols"; then
        echo "!! iOS 静态库缺少全局导出符号 $symbol" >&2
        echo "   （若成为 t/U，链接进 App 后 DynamicLibrary.process() 会解析失败）" >&2
        exit 1
      fi
      echo "✓ $symbol 是全局导出符号"
    done

# ---- 组合任务 ----

# 本地 CI：提交前跑这个
ci: fmt-check lint test check-ios check-ios-sim ios-symbols

# 快速回路
quick: fmt lint test

# ---- Flutter 应用（本机回路：web / Waydroid / 真机） ----

# 生成测试用的中文 EPUB（内容由脚本产出，无版权问题，可随时复现）
spike-epub:
    python3 tools/make_spike_epub.py app/assets/spike_book.epub

# Web 回路的一次性准备：把 Readium JS 拷进 app/web/
# 漏掉这一步会在运行时报 "dart.global.ReadiumReader is not a constructor"
spike-web-setup:
    cd app && dart run flutter_readium:copy_js_file web

# 构建 web 产物
app-web:
    cd app && flutter build web --release

# 编出本机的核心动态库（供 Dart 侧调用）
core-lib:
    cd core && cargo build -p perch-ffi --release

# FFI 端到端冒烟：真的用 Dart 加载 .so 并调用所有 crate（无需设备、无需 Mac）
ffi-smoke: core-lib
    cd dart/perch_core && dart pub get
    cd dart/perch_core && dart run tool/ffi_smoke.dart ../../core/target/release/libperch_ffi.so

# Dart 静态分析
# 注意：这里用 dart analyze 而不是 flutter analyze —— 后者在含中文的项目路径下会
# 抛 FormatException（LSP 通道按字节/字符算错 Content-Length），见 docs/03 环境怪癖
# Flutter 单元测试（纯 Dart VM，不需设备）
app-test:
    cd app && flutter test

app-analyze:
    cd app && dart analyze

# 无头浏览器跑一遍并抓运行时日志（无需图形界面，CI 也能用）
# 用法：先起服务器（python3 -m http.server 8099 -d app/build/web），再跑这个
app-headless-check:
    chromium --headless=new --no-sandbox --disable-dev-shm-usage \
      --enable-unsafe-swiftshader --use-gl=angle --use-angle=swiftshader \
      --enable-logging=stderr --v=0 --virtual-time-budget=60000 --dump-dom \
      http://localhost:8099/ 2>&1 | grep --line-buffered 'perch-spike'
