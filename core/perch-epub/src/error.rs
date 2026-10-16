use thiserror::Error;

/// EPUB 解析错误。
///
/// 每个变体都提供稳定的 [`EpubError::code`]，供跨语言层映射为错误码：
/// 上层可以按错误类型分支处理（例如"文件损坏"提示重新导入、"容器缺失"标记为失败），
/// 而不必依赖随时可能改动的文案。
#[derive(Debug, Error)]
pub enum EpubError {
    /// 文件 I/O 失败。
    #[error("读取文件失败: {0}")]
    Io(#[from] std::io::Error),

    /// 不是合法的 ZIP 容器。
    #[error("不是合法的 ZIP/EPUB 容器: {0}")]
    Zip(#[from] zip::result::ZipError),

    /// 缺少 `META-INF/container.xml`，或其中没有可用的 rootfile。
    #[error("EPUB 缺少 META-INF/container.xml 或其中没有可用的 rootfile")]
    MissingContainer,

    /// 容器内找不到指定条目。
    #[error("容器中找不到条目: {0}")]
    EntryNotFound(String),

    /// XML 语法错误。
    #[error("XML 解析失败: {0}")]
    Xml(String),

    /// OPF 结构不合法（缺少必需内容）。
    #[error("OPF 内容不合法: {0}")]
    InvalidPackage(&'static str),
}

impl EpubError {
    /// 稳定的错误码，跨越语言边界后保持不变。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "EPUB_IO",
            Self::Zip(_) => "EPUB_ZIP",
            Self::MissingContainer => "EPUB_MISSING_CONTAINER",
            Self::EntryNotFound(_) => "EPUB_ENTRY_NOT_FOUND",
            Self::Xml(_) => "EPUB_XML",
            Self::InvalidPackage(_) => "EPUB_INVALID_PACKAGE",
        }
    }
}
