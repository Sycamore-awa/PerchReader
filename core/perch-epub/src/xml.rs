//! 内部 XML 工具。
//!
//! # 为什么只比较"本地名"
//!
//! EPUB 里同一概念有多种合法写法：`dc:title` / `title` / 任意前缀；`opf:package` / `package`。
//! 只比较前缀之后的名字最宽容，也不容易因为作者用了别的前缀而解析失败。
//!
//! 代价是**不校验命名空间 URI**：理论上 `<foo:title>` 声明了错误命名空间也会被接受。
//! 这是划算的取舍（真实 EPUB 的命名空间几乎总是正确的），需要严格校验时再引入 `NsReader`。
//!
//! # quick-xml 0.42 的 API 要点
//!
//! - `QName<'a>` 内部就是 `&'a str`，`QName::as_ref()` 得到 `&str`（不再是字节切片）；
//! - `QName::local_name()` 返回**临时值**，对它取引用再返回会因借用临时值而编译失败，
//!   因此本模块自己按 `:` 切分；
//! - `Attribute::unescape_value()` 已废弃，改用
//!   [`Attribute::normalized_value`]（同时做实体反转义与属性值规范化）。

use std::borrow::Cow;

use quick_xml::XmlVersion;
use quick_xml::events::BytesStart;

/// 取名字中前缀之后的部分（`dc:title` → `title`）。
pub(crate) fn local_name(raw: &str) -> &str {
    match raw.rfind(':') {
        Some(index) => &raw[index + 1..],
        None => raw,
    }
}

/// 元素本地名是否为 `name`。
pub(crate) fn is(element: &BytesStart<'_>, name: &str) -> bool {
    local_name(element.name().into_inner()) == name
}

/// 元素的本地名（调用方通常紧接着 `match`，因此返回借用）。
///
/// 必须用 `into_inner()` 而不是 `as_ref()`：`BytesStart::name()` 返回的是**按值**的
/// `QName`（临时值），对它 `as_ref()` 再返回会变成"返回引用局部临时值"而编译失败；
/// `into_inner()` 直接取出内部的 `&str`，其生命周期来自事件缓冲区，可以安全返回。
pub(crate) fn name_of<'element>(element: &'element BytesStart<'_>) -> &'element str {
    local_name(element.name().into_inner())
}

/// 按本地名查找属性，并做实体反转义与属性值规范化。
///
/// 我们对所有文档一律按 XML 1.0 处理（[`XmlVersion::Implicit1_0`]）：
/// 真实 EPUB 里声明 1.1 的几乎不存在，而 1.0 与 1.1 的差别只在极个别的空白字符上。
///
/// 返回 `String` 而不是 `Cow`：属性的借用生命周期与事件缓冲区交织，
/// 为省一次分配写出复杂的生命周期签名不值得。
pub(crate) fn attr(element: &BytesStart<'_>, name: &str) -> Option<String> {
    for attribute in element.attributes().flatten() {
        if local_name(attribute.key.as_ref()) == name {
            return attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(Cow::into_owned);
        }
    }
    None
}
