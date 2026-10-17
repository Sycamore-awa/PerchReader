/*
 * 链接锚点：强制链接器把 Rust 静态库里对应的目标文件保留下来。
 *
 * 为什么需要它：
 *   `perch_call` / `perch_string_free` 没有任何 Swift/ObjC 代码调用，
 *   默认会被链接器当作死代码丢掉。虽然 podspec 里写了 `-force_load`，
 *   但 `pod_target_xcconfig` 作用在**pod 自己的 target**（静态库，没有链接步骤），
 *   未必能影响 App 的最终链接。
 *
 *   这里对符号**取地址**会产生一个链接期重定位，链接器必须解析它，
 *   于是就会把 Rust 静态库中对应的成员拉进来。这是与语言无关、最可靠的做法。
 *
 * 注意：本文件只做引用，不实现任何逻辑。
 */

#include <stddef.h>

extern char *perch_call(const unsigned char *method, size_t method_len,
                        const unsigned char *args, size_t args_len);
extern void perch_string_free(char *ptr);

/*
 * 若未来没人调用本函数而被优化掉，仍可用 `-u _perch_link_anchor` 强制保留；
 * 目前它被 Swift 插件类调用（见 PerchCoreFfiPlugin.swift），不需要额外开关。
 */
void perch_link_anchor(void) {
    /* 取函数地址即可建立重定位，不需要真的调用 */
    (void)(&perch_call);
    (void)(&perch_string_free);
}
