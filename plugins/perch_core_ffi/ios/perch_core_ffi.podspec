#
# 把 PerchReader 的 Rust 核心静态链接进 Flutter App。
#
# 关键点（结构与 cargokit 官方配方一致，见 docs/09 §D.2）：
#
# 1. `perch_call` 没有任何 Swift/ObjC 代码引用它，不加 -force_load 会被链接器
#    当成死代码丢掉，于是运行时 DynamicLibrary.process() 找不到符号。
#    但 `-force_load` 只能加在 **user_target_xcconfig**（作用于 App 的链接命令行）：
#    静态库 pod 自己没有链接步骤，写在 pod_target_xcconfig 里等于没写。
#    ⚠️ 这一条与 cargokit 公开示例的写法不同，是 CI 实测后的修正（见 docs/10 §四）。
#
# 2. script_phase 必须 execution_position = :before_compile，
#    并声明 input_files / output_files，否则 Xcode 不知道依赖关系，
#    可能"先链接后编译"。
#
# 3. 产物放在 ${BUILT_PRODUCTS_DIR}/libperch_ffi.a，与 -force_load 的路径一致。
#
Pod::Spec.new do |spec|
  spec.name             = 'perch_core_ffi'
  spec.version          = '0.1.0'
  spec.summary          = 'PerchReader 的 Rust 核心（静态链接）'
  spec.description      = <<-DESC
把 core/perch-ffi 编译成 iOS 静态库并链接进 App。
插件本身不提供 MethodChannel —— 它的唯一职责是让 Rust 的 C ABI 符号
出现在最终二进制里，供 dart:ffi 的 DynamicLibrary.process() 解析。
                         DESC
  spec.homepage         = 'https://github.com/Sycamore-awa/PerchReader'
  spec.license          = { :type => 'MIT', :text => 'Copyright PerchReader' }
  spec.author           = { 'PerchReader' => 'noreply@example.com' }
  spec.source           = { :path => '.' }

  spec.source_files     = 'Classes/**/*'
  spec.dependency 'Flutter'
  spec.platform         = :ios, '18.0'

  spec.script_phase = {
    :name => 'Build Rust core',
    :script => '"${PODS_TARGET_SRCROOT}/build_rust.sh"',
    :execution_position => :before_compile,
    :input_files  => ['${PODS_TARGET_SRCROOT}/build_rust.sh'],
    :output_files => ['${BUILT_PRODUCTS_DIR}/libperch_ffi.a'],
  }

  # `DEFINES_MODULE` / `EXCLUDED_ARCHS` 作用在 **pod target** 上
  spec.pod_target_xcconfig = {
    'DEFINES_MODULE' => 'YES',
    'EXCLUDED_ARCHS[sdk=iphonesimulator*]' => 'i386',
  }

  # ⚠️ `-force_load` 必须放在 **user_target_xcconfig**：
  # 静态库 pod 自己没有链接步骤，写在 pod_target_xcconfig 里的链接参数
  # 不会出现在 App 的链接命令行上（CI 实测：Pods-Runner.debug.xcconfig 里没有它，
  # 而 App 链接成功 —— 说明这条参数根本没生效，见 docs/10 §四）。
  # user_target_xcconfig 才会被写进 Pods-Runner.xcconfig，进而作用于 App 的链接。
  spec.user_target_xcconfig = {
    'OTHER_LDFLAGS' => '-force_load ${BUILT_PRODUCTS_DIR}/libperch_ffi.a',
  }
end
