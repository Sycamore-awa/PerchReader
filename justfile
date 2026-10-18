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

    # 必须用 **llvm-nm**：GNU nm 不认识 Mach-O 归档（报 "file format not recognized"）。
    nm_bin=${PERCH_NM_BIN:-}
    if [ -z "$nm_bin" ]; then
      nm_bin=$(command -v llvm-nm 2>/dev/null || true)
    fi
    if [ -z "$nm_bin" ]; then
      nm_bin=$(ls -1 /usr/lib/llvm-*/bin/llvm-nm 2>/dev/null | tail -1 || true)
    fi

    if [ -z "$nm_bin" ]; then
      # 本地可能没装 LLVM。**显式跳过并喊出来**，不静默通过。
      if [ "${PERCH_REQUIRE_LLVM_NM:-0}" = "1" ]; then
        echo "::error::找不到 llvm-nm，无法校验 iOS 符号导出（PERCH_REQUIRE_LLVM_NM=1）" >&2
        exit 1
      fi
      echo "::warning::未找到 llvm-nm，跳过 iOS 符号导出检查（GNU nm 不支持 Mach-O）"
      exit 0
    fi
    echo "使用 nm: $nm_bin"

    # 先整体取出符号表再比较：**不能**写成 `nm ... | grep -q`。
    # grep -q 命中后立即退出会关掉管道，nm 收到 SIGPIPE 而非零退出，
    # 在 set -o pipefail 下整个管道被判失败 —— 表现为"匹配成功却报失败"。
    symbols=$("$nm_bin" --defined-only -g "$lib" 2>/dev/null || true)
    if [ -z "$symbols" ]; then
      echo "::error::$nm_bin 无法读取 $lib 的符号表" >&2
      exit 1
    fi
    for symbol in _perch_call _perch_string_free; do
      if ! grep -q "T $symbol$" <<<"$symbols"; then
        echo "::error::iOS 静态库缺少全局导出符号 $symbol" >&2
        grep -c ' T ' <<<"$symbols" >&2 || true
        exit 1
      fi
      echo "✓ $symbol 是全局导出符号"
    done

# ---- 组合任务 ----

# 本地 CI：提交前跑这个
# 阶段①登记吞吐基准（不是被取消的 CJK 索引测量）
bench-register:
    cd core && cargo run --release -p perch-import --example bench_register

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

# App 的启动自检逻辑，跑在 Linux 的真实 .so 上（Android/iOS 上走的是同一条 FFI 契约）。
# iOS 上的等价物是 CI 的「模拟器冒烟」（docs/12 Step 1）——这里先把逻辑本身验掉，
# 不必等一轮十几分钟的云 CI。
app-selfcheck: core-lib
    cd app && dart run tool/selfcheck_smoke.dart ../core/target/release/libperch_ffi.so

# 用假 xcrun 在 Linux 上验证 CI 的「模拟器冒烟」脚本（通过与失败两条路径）。
# 目的：CI 脚本最容易坏在 shell 细节（SIGPIPE / 退出码 / 找不到命令），
# 而这些在真机上是"一轮十几分钟才知道结果"。
smoke-harness-test:
    bash tools/test_simulator_smoke.sh pass
    bash tools/test_simulator_smoke.sh fail

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
