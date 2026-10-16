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
build-ios-lib:
    cd core && cargo build -p perch-ffi --release --target aarch64-apple-ios

# ---- 组合任务 ----

# 本地 CI：提交前跑这个
ci: fmt-check lint test check-ios check-ios-sim

# 快速回路
quick: fmt lint test
