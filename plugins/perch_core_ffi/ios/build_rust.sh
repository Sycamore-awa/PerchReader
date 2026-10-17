#!/bin/bash
#
# 为当前 Xcode 构建配置编译 Rust 核心静态库，并合并成 fat `.a`。
#
# 由 podspec 的 script_phase 调用（见 perch_core_ffi.podspec）。
#
# 为什么不用 cargokit：
#   cargokit 是一整套 Dart 构建工具链，我们需要的只是「按 target 编译 + lipo」，
#   自己写十来行脚本更透明。podspec 的结构（script_phase + -force_load）仍照
#   cargokit 官方配方（见 docs/09 §D.2）。
#
# ⚠️ macOS 自带 /bin/bash 是 3.2，因此：
#   - 不用关联数组、不用 bash4 语法
#   - 展开可能为空的数组时用 ${arr[@]+"${arr[@]}"} 惯用法（应对 set -u）

set -euo pipefail

: "${PLATFORM_NAME:?缺少 PLATFORM_NAME（应由 Xcode 提供）}"
: "${ARCHS:?缺少 ARCHS（应由 Xcode 提供）}"
: "${CONFIGURATION:?缺少 CONFIGURATION（应由 Xcode 提供）}"
: "${BUILT_PRODUCTS_DIR:?缺少 BUILT_PRODUCTS_DIR（应由 Xcode 提供）}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# 本文件在 plugins/perch_core_ffi/ios/，Rust 工作区在仓库根的 core/
CORE_DIR="${SCRIPT_DIR}/../../../core"

if [ ! -f "${CORE_DIR}/Cargo.toml" ]; then
  echo "error: 找不到 Rust 工作区：${CORE_DIR}" >&2
  exit 1
fi

# Xcode 的 build phase 环境是被裁剪过的，cargo 往往不在 PATH 里
export PATH="${HOME}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:${PATH}"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: 找不到 cargo。macOS 上需要先装 rustup（CI 里由 workflow 安装）。" >&2
  exit 1
fi

# Debug 用 cargo debug（快很多），Release 用 cargo release
CARGO_FLAGS=()
PROFILE_DIR="debug"
if [ "${CONFIGURATION}" = "Release" ]; then
  CARGO_FLAGS+=(--release)
  PROFILE_DIR="release"
fi

LIBS=()
for arch in ${ARCHS}; do
  case "${PLATFORM_NAME}:${arch}" in
    iphoneos:arm64)         target="aarch64-apple-ios" ;;
    iphonesimulator:arm64)  target="aarch64-apple-ios-sim" ;;
    iphonesimulator:x86_64) target="x86_64-apple-ios" ;;
    *)
      echo "error: 不支持的平台/架构组合：${PLATFORM_NAME}:${arch}" >&2
      exit 1
      ;;
  esac

  echo "note: 编译 Rust 核心 target=${target} configuration=${CONFIGURATION}"
  # 必须用 `cargo rustc --crate-type staticlib` 而不是 `cargo build`：
  # perch-ffi 还声明了 cdylib，而 cdylib 需要链接器；在某些环境下会整体失败，
  # 尽管 staticlib 其实已经产出（见 docs/03 §九）。
  (
    cd "${CORE_DIR}"
    cargo rustc -p perch-ffi ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} \
      --target "${target}" --crate-type staticlib
  )

  lib="${CORE_DIR}/target/${target}/${PROFILE_DIR}/libperch_ffi.a"
  if [ ! -f "${lib}" ]; then
    echo "error: 编译后找不到静态库：${lib}" >&2
    exit 1
  fi
  LIBS+=("${lib}")
done

OUT="${BUILT_PRODUCTS_DIR}/libperch_ffi.a"
if [ "${#LIBS[@]}" -eq 1 ]; then
  cp -f "${LIBS[0]}" "${OUT}"
else
  # 多架构（Intel Mac 的模拟器会同时要 arm64 + x86_64）合并成 fat .a
  # cargokit 也是这么做的（不是 xcframework）
  xcrun lipo -create ${LIBS[@]+"${LIBS[@]}"} -output "${OUT}"
fi

echo "note: Rust 静态库已就绪 -> ${OUT}"
