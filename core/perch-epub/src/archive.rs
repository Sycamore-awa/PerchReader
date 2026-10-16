//! ZIP 容器层：读条目、找 OPF。
//!
//! 这一层只做"容器"的事，不认识 EPUB 的语义（元数据、书脊等由 [`crate::package`] 负责）。

use std::io::{Cursor, Read, Seek};

use quick_xml::events::Event;
use zip::ZipArchive;

use crate::error::EpubError;
use crate::xml;

/// EPUB 容器描述文件在 ZIP 内的固定路径。
pub const CONTAINER_PATH: &str = "META-INF/container.xml";

/// OPF 的标准媒体类型。
const OPF_MEDIA_TYPE: &str = "application/oebps-package+xml";

/// EPUB 容器（本质是 ZIP）。
///
/// 泛型 `R` 让同一份实现同时服务于两种场景：
/// - [`EpubArchive::open`]：真实文件，流式读取，**不把整本书读进内存**；
/// - [`EpubArchive::from_bytes`]：内存字节，用于测试与"下载后先解析"。
pub struct EpubArchive<R: Read + Seek> {
    zip: ZipArchive<R>,
    names: Vec<String>,
}

impl EpubArchive<std::fs::File> {
    /// 打开磁盘上的 EPUB 文件。
    ///
    /// # Errors
    ///
    /// 文件不存在/无权限 → [`EpubError::Io`]；不是合法 ZIP → [`EpubError::Zip`]。
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, EpubError> {
        let file = std::fs::File::open(path)?;
        Self::new(file)
    }
}

impl EpubArchive<Cursor<Vec<u8>>> {
    /// 由内存字节构造。
    ///
    /// # Errors
    ///
    /// 不是合法 ZIP → [`EpubError::Zip`]。
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, EpubError> {
        Self::new(Cursor::new(bytes))
    }
}

impl<R: Read + Seek> EpubArchive<R> {
    /// 包装一个可寻址的读取器。
    ///
    /// # Errors
    ///
    /// 不是合法 ZIP → [`EpubError::Zip`]。
    pub fn new(reader: R) -> Result<Self, EpubError> {
        let zip = ZipArchive::new(reader)?;
        // zip 9 的 `file_names()` 产出 `Result<Cow<str>, ZipError>`——条目名来自中心目录，
        // 理论上可能损坏，因此老实传播错误而不是静默丢弃。
        let mut names = Vec::with_capacity(zip.len());
        for name in zip.file_names() {
            names.push(name?.into_owned());
        }
        Ok(Self { zip, names })
    }

    /// 容器内全部条目名（不带前导 `/`）。
    #[must_use]
    pub fn entry_names(&self) -> &[String] {
        &self.names
    }

    /// 容器内是否存在该条目。
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.names.iter().any(|existing| existing == name)
    }

    /// 读取条目全部字节。
    ///
    /// `name` 是 **ZIP 条目名**（不带前导 `/`），与 [`crate::ManifestItem::entry_name`] 配套使用。
    ///
    /// # Errors
    ///
    /// 条目不存在 → [`EpubError::EntryNotFound`]；读取失败 → [`EpubError::Io`]。
    pub fn read(&mut self, name: &str) -> Result<Vec<u8>, EpubError> {
        let mut entry = self.zip.by_name(name).map_err(|err| match err {
            zip::result::ZipError::FileNotFound => EpubError::EntryNotFound(name.to_owned()),
            other => EpubError::Zip(other),
        })?;

        // 用条目声明的长度预分配，避免反复扩容；长度不可信时退回 0（不做乐观假设）
        let hint = usize::try_from(entry.size()).unwrap_or(0);
        let mut buffer = Vec::with_capacity(hint);
        entry.read_to_end(&mut buffer)?;
        Ok(buffer)
    }

    /// 读取条目并当作 UTF-8 文本。
    ///
    /// 非法 UTF-8 字节会被替换为 U+FFFD 而**不是**报错：
    /// 个别老书的 XML 声明与实际编码不一致，宁可降级也不要让整本书打不开。
    ///
    /// # Errors
    ///
    /// 同 [`EpubArchive::read`]。
    pub fn read_to_string(&mut self, name: &str) -> Result<String, EpubError> {
        let bytes = self.read(name)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// 解析 `META-INF/container.xml`，返回 OPF 的 **ZIP 条目名**（如 `OEBPS/content.opf`）。
    ///
    /// 规范要求取第一个 `media-type="application/oebps-package+xml"` 的 rootfile；
    /// 若整本都没标注 media-type，则宽容地退回第一个 rootfile
    /// （真实世界存在不合规的书，因为一个属性就打不开不值得）。
    ///
    /// # Errors
    ///
    /// 缺少 container.xml 或其中没有 rootfile → [`EpubError::MissingContainer`]。
    pub fn opf_path(&mut self) -> Result<String, EpubError> {
        // 先显式区分"根本没有容器描述文件"与"某个资源读不到"：
        // 前者说明这不是一个合法 EPUB（值得给用户单独的提示），后者只是资源缺失。
        if !self.contains(CONTAINER_PATH) {
            return Err(EpubError::MissingContainer);
        }

        let container_xml = self.read_to_string(CONTAINER_PATH)?;
        let mut reader = quick_xml::Reader::from_str(&container_xml);
        reader.config_mut().trim_text(true);

        let mut first: Option<String> = None;
        let mut preferred: Option<String> = None;

        loop {
            match reader.read_event() {
                Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                    if !xml::is(&element, "rootfile") {
                        continue;
                    }
                    let Some(full_path) = xml::attr(&element, "full-path") else {
                        continue;
                    };
                    let media_type = xml::attr(&element, "media-type");
                    if preferred.is_none() && media_type.as_deref() == Some(OPF_MEDIA_TYPE) {
                        preferred = Some(full_path.clone());
                    }
                    if first.is_none() {
                        first = Some(full_path);
                    }
                }
                Ok(Event::Eof) => break,
                Err(err) => return Err(EpubError::Xml(err.to_string())),
                _ => {}
            }
        }

        preferred.or(first).ok_or(EpubError::MissingContainer)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::test_fixture;

    #[test]
    fn opens_synthetic_epub_and_lists_entries() {
        let archive = EpubArchive::from_bytes(test_fixture::minimal_epub()).unwrap();
        let names = archive.entry_names();
        assert!(
            names.iter().any(|n| n == CONTAINER_PATH),
            "names = {names:?}"
        );
        assert!(names.iter().any(|n| n == "OEBPS/content.opf"));
        assert!(archive.contains("OEBPS/text/chap01.xhtml"));
        assert!(!archive.contains("OEBPS/text/missing.xhtml"));
    }

    #[test]
    fn resolves_opf_path_from_container_xml() {
        let mut archive = EpubArchive::from_bytes(test_fixture::minimal_epub()).unwrap();
        assert_eq!(archive.opf_path().unwrap(), "OEBPS/content.opf");
    }

    #[test]
    fn reads_entry_as_text() {
        let mut archive = EpubArchive::from_bytes(test_fixture::minimal_epub()).unwrap();
        let opf = archive.read_to_string("OEBPS/content.opf").unwrap();
        assert!(opf.contains("测试书籍"), "opf = {opf}");
    }

    #[test]
    fn missing_container_is_reported() {
        let mut archive = EpubArchive::from_bytes(test_fixture::epub_without_container()).unwrap();
        let err = archive.opf_path().unwrap_err();
        assert_eq!(err.code(), "EPUB_MISSING_CONTAINER");
    }

    #[test]
    fn missing_entry_is_reported() {
        let mut archive = EpubArchive::from_bytes(test_fixture::minimal_epub()).unwrap();
        let err = archive.read("OEBPS/nope.xhtml").unwrap_err();
        assert_eq!(err.code(), "EPUB_ENTRY_NOT_FOUND");
    }

    #[test]
    fn garbage_bytes_are_rejected() {
        let result = EpubArchive::from_bytes(b"this is definitely not a zip".to_vec());
        assert!(result.is_err(), "非 ZIP 字节应当被拒绝");
        // 用 err() 而非 unwrap_err()：EpubArchive 没有实现 Debug
        assert_eq!(result.err().unwrap().code(), "EPUB_ZIP");
    }

    #[test]
    fn container_without_media_type_falls_back_to_first_rootfile() {
        // 不合规的书：rootfile 没标 media-type，仍应能打开
        let container = r#"<?xml version="1.0"?>
<container><rootfiles><rootfile full-path="OPS/book.opf"/></rootfiles></container>"#;
        let bytes = test_fixture::build(&[
            ("META-INF/container.xml", container.as_bytes()),
            ("OPS/book.opf", b"<package/>"),
        ]);
        let mut archive = EpubArchive::from_bytes(bytes).unwrap();
        assert_eq!(archive.opf_path().unwrap(), "OPS/book.opf");
    }

    #[test]
    fn non_utf8_content_degrades_gracefully() {
        let bytes = test_fixture::build(&[("OEBPS/raw.bin", &[0xff, 0xfe, 0x41])]);
        let mut archive = EpubArchive::from_bytes(bytes).unwrap();
        // 不应报错：非法字节被替换
        let text = archive.read_to_string("OEBPS/raw.bin").unwrap();
        assert!(!text.is_empty());
    }
}
