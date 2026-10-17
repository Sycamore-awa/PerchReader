//! OPF（package document）解析：元数据、清单、书脊。
//!
//! 解析策略是**单遍流式**的：不建 DOM、不递归，遇到元素就更新少量状态机变量。
//! 这对"打开 <1s"的目标有意义——一本 1000 章的书，OPF 的清单可能有 3000+ 个 `item`，
//! 建树再遍历会比直接流过贵一个量级。

use quick_xml::events::{BytesStart, Event};

use crate::error::EpubError;
use crate::xml;

/// 书籍元数据（对应 OPF 的 `metadata`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    /// 书名（`dc:title`，取第一个非空的）。
    pub title: Option<String>,
    /// 作者列表（`dc:creator`，可以有多个，按出现顺序）。
    pub creators: Vec<String>,
    /// 语言（`dc:language`，BCP-47）。
    pub language: Option<String>,
    /// 标识符（`dc:identifier`，通常是 UUID 或 ISBN）。
    pub identifier: Option<String>,
    /// 出版社。
    pub publisher: Option<String>,
    /// 简介。
    pub description: Option<String>,
    /// EPUB2 的封面声明：`<meta name="cover" content="..."/>` 里的 `content`，
    /// 是清单项的 **id**（不是路径）。要拿到封面路径需再用
    /// [`Package::item_by_id`] 换成 [`ManifestItem::href`]，
    /// 或直接用 [`Package::cover_image`]。
    ///
    /// 这是 **EPUB2** 的写法。EPUB3 把封面写成清单项上的
    /// `properties="cover-image"`，不在元数据里——两者都由
    /// [`Package::cover_image`] 统一处理。
    ///
    /// 本结构体不派生 serde（`perch-epub` 不依赖 serde），
    /// 对外序列化时由调用方决定字段名。
    pub cover_id: Option<String>,
}

impl Metadata {
    /// 用于 UI 展示的标题：标题缺失或只有空白时兜底为"未命名"。
    #[must_use]
    pub fn display_title(&self) -> &str {
        match self.title.as_deref() {
            Some(title) if !title.trim().is_empty() => title,
            _ => "未命名",
        }
    }
}

/// 清单项（`manifest` 中的一个 `item`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestItem {
    /// 清单 id。
    pub id: String,
    /// **容器绝对路径**（带前导 `/`），已按 OPF 所在目录解析完毕。
    pub href: String,
    /// 媒体类型。
    pub media_type: Option<String>,
    /// `properties` 拆出的多个值（如 `nav` / `scripted` / `mathml`）。
    pub properties: Vec<String>,
}

impl ManifestItem {
    /// 是否声明了指定 property。
    #[must_use]
    pub fn has_property(&self, property: &str) -> bool {
        self.properties.iter().any(|value| value == property)
    }

    /// 是否为 HTML/XHTML 内容文档。
    #[must_use]
    pub fn is_xhtml(&self) -> bool {
        matches!(
            self.media_type.as_deref(),
            Some("application/xhtml+xml" | "text/html")
        )
    }

    /// 对应的 **ZIP 条目名**（去掉前导 `/`），可直接交给
    /// [`crate::EpubArchive::read`]。
    #[must_use]
    pub fn entry_name(&self) -> &str {
        self.href.strip_prefix('/').unwrap_or(&self.href)
    }
}

/// 书脊项（`spine` 中的一个 `itemref`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpineItem {
    /// 指向清单项的 id。
    pub idref: String,
    /// 是否为线性内容（`linear="no"` 表示非线性，如封面、附录）。
    pub linear: bool,
}

/// OPF 的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// OPF 的 `version`（`2.0` / `3.0`）——用它判断目录该走 NAV 还是 NCX。
    pub version: Option<String>,
    /// 元数据。
    pub metadata: Metadata,
    /// 清单。
    pub manifest: Vec<ManifestItem>,
    /// 书脊（阅读顺序）。
    pub spine: Vec<SpineItem>,
    /// `spine` 的 `toc` 属性：EPUB2 里指向 NCX 的清单 id。
    pub toc_idref: Option<String>,
    /// OPF 所在目录的容器路径（带前导 `/`，如 `/OEBPS`）。
    pub base_dir: String,
}

impl Package {
    /// 解析 OPF 文档。
    ///
    /// `opf_entry_name` 是 OPF 在 ZIP 内的条目名（如 `OEBPS/content.opf`），
    /// 用于把清单里的相对 `href` 解析成容器绝对路径。
    ///
    /// # Errors
    ///
    /// - XML 语法错误 → [`EpubError::Xml`]
    /// - `manifest` 为空 → [`EpubError::InvalidPackage`]
    ///
    /// 刻意**不**要求 `spine` 非空：确实存在只有清单没有书脊的坏书，
    /// 与其打不开，不如让它进来、由上层决定怎么提示。
    pub fn parse(opf_entry_name: &str, opf_xml: &str) -> Result<Self, EpubError> {
        let base_dir = container_dir_of(opf_entry_name);

        let mut reader = quick_xml::Reader::from_str(opf_xml);
        reader.config_mut().trim_text(true);

        let mut state = ParseState::default();

        loop {
            match reader.read_event() {
                Ok(Event::Start(element)) => state.on_element(&element, &base_dir, true),
                Ok(Event::Empty(element)) => state.on_element(&element, &base_dir, false),
                Ok(Event::Text(text)) => {
                    // quick-xml 0.42 的 BytesText 解引用为 str（原始、未反转义的文本），
                    // 实体反转义改由 escape::unescape 负责。
                    let raw: &str = &text;
                    if let Ok(value) = quick_xml::escape::unescape(raw) {
                        state.on_text(&value);
                    }
                }
                Ok(Event::End(element)) => {
                    // `BytesEnd::name()` 返回的是**临时值**（见 crate::xml 的说明）：
                    // 在这个块内把本地名用掉，不要传出块外。
                    let name = element.name();
                    state.on_end(xml::local_name(name.as_ref()));
                }
                Ok(Event::Eof) => break,
                Err(err) => return Err(EpubError::Xml(err.to_string())),
                _ => {}
            }
        }

        if state.manifest.is_empty() {
            return Err(EpubError::InvalidPackage("manifest 为空"));
        }

        Ok(Self {
            version: state.version,
            metadata: state.metadata,
            manifest: state.manifest,
            spine: state.spine,
            toc_idref: state.toc_idref,
            base_dir,
        })
    }

    /// 按清单 id 查找清单项。
    #[must_use]
    pub fn item_by_id(&self, id: &str) -> Option<&ManifestItem> {
        self.manifest.iter().find(|item| item.id == id)
    }

    /// 按书脊顺序解析出清单项。
    ///
    /// `idref` 指向不存在的清单项时**跳过**（坏书常见现象，不应导致整本打不开）。
    #[must_use]
    pub fn spine_items(&self) -> Vec<&ManifestItem> {
        self.spine
            .iter()
            .filter_map(|item| self.item_by_id(&item.idref))
            .collect()
    }

    /// EPUB3 的导航文档（`properties` 含 `nav`）。
    #[must_use]
    pub fn nav_item(&self) -> Option<&ManifestItem> {
        self.manifest.iter().find(|item| item.has_property("nav"))
    }

    /// EPUB2 的 NCX 目录文档：优先按 `spine@toc` 找，找不到再按媒体类型兜底。
    #[must_use]
    pub fn ncx_item(&self) -> Option<&ManifestItem> {
        if let Some(idref) = &self.toc_idref {
            if let Some(item) = self.item_by_id(idref) {
                return Some(item);
            }
        }
        self.manifest
            .iter()
            .find(|item| item.media_type.as_deref() == Some("application/x-dtbncx+xml"))
    }

    /// 封面在容器内的绝对路径：优先 EPUB3 的 `properties="cover-image"`，
    /// 其次 EPUB2 的 `<meta name="cover">` 指向的 manifest id。
    /// 两者都没有时返回 `None` —— 不猜。
    ///
    /// 返回值是清单项的 [`href`](ManifestItem::href)，即**容器绝对路径**
    /// （带前导 `/`），可直接交给 [`crate::EpubArchive::read`]。
    ///
    /// 两条路径都不做"启发式"：不把清单里的第一张图片当封面（那多半是
    /// 出版社 logo 或装饰图），也不按 id 名字猜（`cover` / `cover-image` 之类）。
    ///
    /// EPUB2 分支会检查 `media-type`：`<meta name="cover">` 有少数情况下指向
    /// 一个 **XHTML 封面页**而非图片，那种 href 不能当缩略图用，此时返回 `None`。
    #[must_use]
    pub fn cover_image(&self) -> Option<&str> {
        // EPUB3：封面是清单项上的属性，规范写法定死了，最可靠，优先采用。
        if let Some(item) = self
            .manifest
            .iter()
            .find(|item| item.has_property("cover-image"))
        {
            return Some(item.href.as_str());
        }

        // EPUB2：`<meta name="cover" content="清单 id"/>` 只给了 id，需要回到清单里找。
        // id 悬空（指向不存在的清单项）时返回 None，而不是退而求其次挑一个别的项。
        let id = self.metadata.cover_id.as_deref()?;
        let item = self.item_by_id(id)?;
        // 少数 EPUB2 把 `name="cover"` 指向一个 **XHTML 封面页**而不是图片。
        // 把那个 href 当缩略图会渲染成文字/坏图，所以只在**能确定不是图片**时拒绝；
        // media-type 缺失（None）无从判断，放行。
        if let Some(media_type) = item.media_type.as_deref()
            && !media_type.starts_with("image/")
        {
            return None;
        }
        Some(item.href.as_str())
    }
}

/// 把相对 `href` 解析为**容器绝对路径**（带前导 `/`）。
///
/// - 相对路径基于 `base_dir` 解析，处理 `.` 与 `..`
/// - 以 `/` 开头的路径视为容器根绝对路径
/// - `#片段` 与 `?查询串` 原样保留（它们对锚点定位有意义）
/// - `..` 越过根时不会 panic，也不会跑到根以外
#[must_use]
pub fn resolve_href(base_dir: &str, href: &str) -> String {
    let (path, suffix) = match href.find(['#', '?']) {
        Some(index) => (&href[..index], &href[index..]),
        None => (href, ""),
    };
    let is_absolute = path.starts_with('/');

    let mut segments: Vec<&str> = Vec::new();
    if !is_absolute {
        segments.extend(
            base_dir
                .split('/')
                .filter(|segment| !segment.is_empty() && *segment != "."),
        );
    }
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }

    let mut resolved = String::with_capacity(href.len() + 1);
    resolved.push('/');
    resolved.push_str(&segments.join("/"));
    resolved.push_str(suffix);
    resolved
}

/// 由 OPF 的 ZIP 条目名推出它所在目录的容器路径。
///
/// `OEBPS/content.opf` → `/OEBPS`；`content.opf` → `/`。
#[must_use]
pub fn container_dir_of(opf_entry_name: &str) -> String {
    match opf_entry_name.rfind('/') {
        Some(index) => {
            let directory = opf_entry_name[..index].trim_matches('/');
            format!("/{directory}")
        }
        None => "/".to_owned(),
    }
}

/// OPF 里我们关心的区段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Section {
    #[default]
    None,
    Metadata,
    Manifest,
    Spine,
}

/// `metadata` 里我们关心的字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Creator,
    Language,
    Identifier,
    Publisher,
    Description,
}

/// 解析过程中的可变状态。
#[derive(Debug, Default)]
struct ParseState {
    section: Section,
    field: Option<Field>,
    buffer: String,
    version: Option<String>,
    metadata: Metadata,
    manifest: Vec<ManifestItem>,
    spine: Vec<SpineItem>,
    toc_idref: Option<String>,
}

impl ParseState {
    /// 处理元素开始（`is_start` 为 `false` 表示自闭合元素）。
    fn on_element(&mut self, element: &BytesStart<'_>, base_dir: &str, is_start: bool) {
        match xml::name_of(element) {
            "package" => self.version = xml::attr(element, "version"),

            // 只有"非自闭合"的区段元素才进入对应状态
            "metadata" if is_start => self.section = Section::Metadata,
            "manifest" if is_start => self.section = Section::Manifest,
            "spine" if is_start => {
                self.section = Section::Spine;
                self.toc_idref = xml::attr(element, "toc");
            }

            // 属性型元素：自闭合与带子元素都要处理
            "item" => {
                if self.section == Section::Manifest {
                    if let Some(item) = manifest_item(element, base_dir) {
                        self.manifest.push(item);
                    }
                }
            }
            "itemref" => {
                if self.section == Section::Spine {
                    if let Some(idref) = xml::attr(element, "idref") {
                        let linear = xml::attr(element, "linear").as_deref() != Some("no");
                        self.spine.push(SpineItem { idref, linear });
                    }
                }
            }

            // EPUB2 的封面声明：`<meta name="cover" content="清单 id"/>`。
            // 必须排在下面"元数据文本字段"的通配分支之前——`meta` 不是文本字段，
            // 走通配分支只会白跑一趟 `metadata_field`；同时这里不区分自闭合与否，
            // `<meta .../>` 与 `<meta ...></meta>` 两种写法都能接住。
            "meta" if self.section == Section::Metadata => {
                if let Some(id) = cover_meta_id(element) {
                    // 与其它单值元数据字段一致：多份声明时第一份生效
                    set_if_none(&mut self.metadata.cover_id, id);
                }
            }

            // 元数据字段：开始收集文本
            name if is_start && self.section == Section::Metadata => {
                if let Some(field) = metadata_field(name) {
                    self.field = Some(field);
                    self.buffer.clear();
                }
            }
            _ => {}
        }
    }

    /// 处理文本节点（只在我们正等待某个元数据字段时收集）。
    fn on_text(&mut self, text: &str) {
        if self.field.is_some() {
            self.buffer.push_str(text);
        }
    }

    /// 处理元素结束。
    fn on_end(&mut self, local_name: &str) {
        if let Some(field) = self.field {
            if metadata_field(local_name) == Some(field) {
                let value = self.buffer.trim().to_owned();
                self.field = None;
                self.buffer.clear();
                if !value.is_empty() {
                    self.commit(field, value);
                }
            }
            return;
        }
        if matches!(local_name, "metadata" | "manifest" | "spine") {
            self.section = Section::None;
        }
    }

    /// 写入元数据字段。
    fn commit(&mut self, field: Field, value: String) {
        match field {
            // 单值字段取第一个（EPUB 规范：`dc:title` 等应以第一个为准）
            Field::Title => set_if_none(&mut self.metadata.title, value),
            Field::Language => set_if_none(&mut self.metadata.language, value),
            Field::Identifier => set_if_none(&mut self.metadata.identifier, value),
            Field::Publisher => set_if_none(&mut self.metadata.publisher, value),
            Field::Description => set_if_none(&mut self.metadata.description, value),
            // 作者可以有多个，全部保留
            Field::Creator => self.metadata.creators.push(value),
        }
    }
}

/// 仅在字段为空时写入（"第一个生效"语义）。
fn set_if_none(slot: &mut Option<String>, value: String) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

/// 把元素本地名映射到元数据字段。
fn metadata_field(name: &str) -> Option<Field> {
    match name {
        "title" => Some(Field::Title),
        "creator" => Some(Field::Creator),
        "language" => Some(Field::Language),
        "identifier" => Some(Field::Identifier),
        "publisher" => Some(Field::Publisher),
        "description" => Some(Field::Description),
        _ => None,
    }
}

/// 从 `<meta>` 元素里取 **EPUB2** 封面声明指向的清单 id。
///
/// 只认 `<meta name="cover" content="..."/>` 这一种写法，其余 `<meta>` 一律返回
/// `None`。两种 `meta` 的区分方式：
///
/// - **EPUB2** 用 `name` + `content`：`name` 是被声明的元数据名，`content` 是值；
///   `content` 在这里是 manifest item 的 **id**。
/// - **EPUB3** 用 `property` + 元素文本：`<meta property="dcterms:modified">...</meta>`，
///   根本没有 `name` 属性。因此**只读 `name`、从不读 `property`**，
///   `<meta property="cover">` 不会、也不能被当成 EPUB2 的封面声明。
///
/// 为了多一层保险：元素上只要出现 `property` 属性就按 EPUB3 处理、直接跳过，
/// 即使它同时带着 `name="cover"` 也不认（真实书里没有这种混写）。
///
/// 大小写：**属性名**严格按 XML 规则大小写敏感（`NAME="cover"` 不认）；但 `cover`
/// 这个**值**放宽为 ASCII 大小写不敏感——实践中确有生成器写成 `name="Cover"`，
/// 而 `cover` 不是专有名词，这个宽容不会和别的东西撞车。
fn cover_meta_id(element: &BytesStart<'_>) -> Option<String> {
    if xml::attr(element, "property").is_some() {
        return None;
    }
    let name = xml::attr(element, "name")?;
    if !name.trim().eq_ignore_ascii_case("cover") {
        return None;
    }
    let content = xml::attr(element, "content")?;
    let content = content.trim();
    (!content.is_empty()).then(|| content.to_owned())
}

/// 解析一个清单项；缺 `id` 或 `href` 时视为无效并跳过。
fn manifest_item(element: &BytesStart<'_>, base_dir: &str) -> Option<ManifestItem> {
    let id = xml::attr(element, "id")?;
    let href = xml::attr(element, "href")?;
    let media_type = xml::attr(element, "media-type");
    let properties = xml::attr(element, "properties")
        .map(|raw| raw.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    Some(ManifestItem {
        id,
        href: resolve_href(base_dir, &href),
        media_type,
        properties,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::test_fixture;

    fn package() -> Package {
        Package::parse("OEBPS/content.opf", test_fixture::OPF).unwrap()
    }

    #[test]
    fn parses_metadata() {
        let package = package();
        assert_eq!(package.version.as_deref(), Some("3.0"));
        assert_eq!(package.metadata.display_title(), "测试书籍");
        assert_eq!(package.metadata.creators, vec!["张三", "李四"]);
        assert_eq!(package.metadata.language.as_deref(), Some("zh-Hans"));
        assert_eq!(package.metadata.publisher.as_deref(), Some("测试出版社"));
        assert_eq!(
            package.metadata.description.as_deref(),
            Some("一本书架上的假书。")
        );
        assert!(
            package
                .metadata
                .identifier
                .as_deref()
                .is_some_and(|id| id.starts_with("urn:uuid:"))
        );
    }

    #[test]
    fn resolves_manifest_hrefs_against_opf_dir() {
        let package = package();
        assert_eq!(package.base_dir, "/OEBPS");
        assert_eq!(
            package.item_by_id("chap1").unwrap().href,
            "/OEBPS/text/chap01.xhtml"
        );
        // 多余的 `./` 被规范化
        assert_eq!(
            package.item_by_id("chap2").unwrap().href,
            "/OEBPS/text/chap02.xhtml"
        );
        // `../` 越过 OPF 所在目录
        assert_eq!(package.item_by_id("css").unwrap().href, "/styles/main.css");
        // ZIP 条目名不带前导斜杠
        assert_eq!(
            package.item_by_id("chap1").unwrap().entry_name(),
            "OEBPS/text/chap01.xhtml"
        );
        assert_eq!(package.manifest.len(), 6);
    }

    #[test]
    fn parses_multi_valued_properties() {
        let package = package();
        let chapter = package.item_by_id("chap3").unwrap();
        assert_eq!(chapter.properties, vec!["scripted", "mathml"]);
        assert!(chapter.has_property("mathml"));
        assert!(!chapter.has_property("nav"));
        assert!(chapter.is_xhtml());
    }

    #[test]
    fn parses_spine_order_and_linear_flag() {
        let package = package();
        assert_eq!(package.spine.len(), 3);
        assert_eq!(package.spine[0].idref, "chap1");
        assert!(package.spine[0].linear);
        assert_eq!(package.spine[2].idref, "chap3");
        assert!(!package.spine[2].linear, "linear=\"no\" 应被识别为非线性");

        let ordered: Vec<&str> = package
            .spine_items()
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        assert_eq!(ordered, vec!["chap1", "chap2", "chap3"]);
    }

    #[test]
    fn finds_nav_and_ncx_documents() {
        let package = package();
        assert_eq!(package.nav_item().unwrap().id, "nav");
        assert_eq!(package.toc_idref.as_deref(), Some("ncx"));
        assert_eq!(package.ncx_item().unwrap().id, "ncx");
    }

    #[test]
    fn ncx_falls_back_to_media_type_when_toc_attr_missing() {
        let xml = r#"<package><manifest>
            <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
        </manifest><spine><itemref idref="ncx"/></spine></package>"#;
        let package = Package::parse("content.opf", xml).unwrap();
        assert!(package.toc_idref.is_none());
        assert_eq!(package.ncx_item().unwrap().id, "ncx");
    }

    #[test]
    fn dangling_spine_idref_is_skipped() {
        let xml = r#"<package><manifest>
            <item id="a" href="a.xhtml" media-type="application/xhtml+xml"/>
        </manifest><spine>
            <itemref idref="a"/><itemref idref="ghost"/>
        </spine></package>"#;
        let package = Package::parse("content.opf", xml).unwrap();
        assert_eq!(package.spine.len(), 2, "书脊本身保留原始两项");
        assert_eq!(package.spine_items().len(), 1, "解析不到的那项被跳过");
    }

    #[test]
    fn first_title_wins_but_all_creators_are_kept() {
        let xml = r#"<package><metadata>
            <dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">第一</dc:title>
            <dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">第二</dc:title>
            <dc:creator xmlns:dc="http://purl.org/dc/elements/1.1/">甲</dc:creator>
            <dc:creator xmlns:dc="http://purl.org/dc/elements/1.1/">乙</dc:creator>
        </metadata><manifest><item id="a" href="a.xhtml"/></manifest></package>"#;
        let package = Package::parse("content.opf", xml).unwrap();
        assert_eq!(package.metadata.title.as_deref(), Some("第一"));
        assert_eq!(package.metadata.creators, vec!["甲", "乙"]);
    }

    #[test]
    fn empty_manifest_is_rejected() {
        let err =
            Package::parse("content.opf", "<package><manifest></manifest></package>").unwrap_err();
        assert_eq!(err.code(), "EPUB_INVALID_PACKAGE");
    }

    #[test]
    fn malformed_xml_is_reported() {
        let err =
            Package::parse("content.opf", "<package><manifest></spine></package>").unwrap_err();
        assert_eq!(err.code(), "EPUB_XML");
    }

    #[test]
    fn href_resolution_handles_edge_cases() {
        assert_eq!(resolve_href("/OEBPS", "a.xhtml"), "/OEBPS/a.xhtml");
        assert_eq!(resolve_href("/OEBPS", "./a.xhtml"), "/OEBPS/a.xhtml");
        assert_eq!(resolve_href("/OEBPS", "../a.xhtml"), "/a.xhtml");
        assert_eq!(
            resolve_href("/OEBPS/text", "../img/cover.jpg"),
            "/OEBPS/img/cover.jpg"
        );
        assert_eq!(resolve_href("/", "a.xhtml"), "/a.xhtml");
        // 以 / 开头视为容器根
        assert_eq!(resolve_href("/OEBPS", "/root.css"), "/root.css");
        // 片段与查询串保留
        assert_eq!(resolve_href("/OEBPS", "c.xhtml#s1"), "/OEBPS/c.xhtml#s1");
        assert_eq!(resolve_href("/OEBPS", "c.xhtml?v=1"), "/OEBPS/c.xhtml?v=1");
        // 越过根不 panic，也不会跑出根
        assert_eq!(resolve_href("/OEBPS", "../../../a.xhtml"), "/a.xhtml");
    }

    #[test]
    fn container_dir_of_handles_root_and_nesting() {
        assert_eq!(container_dir_of("content.opf"), "/");
        assert_eq!(container_dir_of("OEBPS/content.opf"), "/OEBPS");
        assert_eq!(container_dir_of("a/b/c/content.opf"), "/a/b/c");
        assert_eq!(container_dir_of("/OEBPS/content.opf"), "/OEBPS");
    }

    #[test]
    fn display_title_falls_back_when_blank() {
        let metadata = Metadata::default();
        assert_eq!(metadata.display_title(), "未命名");

        let metadata = Metadata {
            title: Some("   ".to_owned()),
            ..Metadata::default()
        };
        assert_eq!(metadata.display_title(), "未命名");
    }

    /// 拼一个只关心封面所需的 OPF：`metadata` 与 `manifest` 由调用方给片段。
    fn cover_package(metadata: &str, manifest: &str) -> Package {
        let xml = format!(
            r#"<package version="2.0"><metadata>{metadata}</metadata><manifest>{manifest}</manifest></package>"#
        );
        Package::parse("OEBPS/content.opf", &xml).unwrap()
    }

    #[test]
    fn epub3_cover_image_property_is_returned() {
        let package = cover_package(
            "",
            r#"<item id="chap" href="text/chap1.xhtml" media-type="application/xhtml+xml"/>
               <item id="cov" href="images/cover.jpg" media-type="image/jpeg" properties="cover-image"/>"#,
        );
        assert_eq!(package.cover_image(), Some("/OEBPS/images/cover.jpg"));
    }

    #[test]
    fn epub2_meta_cover_resolves_manifest_id_to_href() {
        let package = cover_package(
            r#"<meta name="cover" content="cov"/>"#,
            r#"<item id="chap" href="text/chap1.xhtml" media-type="application/xhtml+xml"/>
               <item id="cov" href="images/cover.jpg" media-type="image/jpeg"/>"#,
        );
        // meta 里存的是 id，不是路径
        assert_eq!(package.metadata.cover_id.as_deref(), Some("cov"));
        // 对外给的是容器绝对路径
        assert_eq!(package.cover_image(), Some("/OEBPS/images/cover.jpg"));
    }

    #[test]
    fn epub3_cover_wins_over_epub2_meta() {
        let package = cover_package(
            r#"<meta name="cover" content="old-cover"/>"#,
            r#"<item id="old-cover" href="images/old.jpg" media-type="image/jpeg"/>
               <item id="new-cover" href="images/new.jpg" media-type="image/jpeg" properties="cover-image"/>"#,
        );
        assert_eq!(package.metadata.cover_id.as_deref(), Some("old-cover"));
        assert_eq!(
            package.cover_image(),
            Some("/OEBPS/images/new.jpg"),
            "properties=\"cover-image\" 优先于 EPUB2 的 meta"
        );
    }

    #[test]
    fn cover_is_none_when_nothing_declares_it() {
        // 清单里明明有图片、id 甚至就叫 `cover`，但没有声明就不能猜
        let package = cover_package(
            "",
            r#"<item id="cover" href="images/cover.jpg" media-type="image/jpeg"/>
               <item id="chap" href="text/chap1.xhtml" media-type="application/xhtml+xml"/>"#,
        );
        assert!(package.metadata.cover_id.is_none());
        assert_eq!(
            package.cover_image(),
            None,
            "不能把第一张图片 / 名字像封面的项当封面"
        );
    }

    #[test]
    fn dangling_cover_id_returns_none() {
        let package = cover_package(
            r#"<meta name="cover" content="ghost"/>"#,
            r#"<item id="cover" href="images/cover.jpg" media-type="image/jpeg"/>"#,
        );
        assert_eq!(package.metadata.cover_id.as_deref(), Some("ghost"));
        assert_eq!(
            package.cover_image(),
            None,
            "id 悬空时返回 None，不能退回去挑一个相近的项"
        );
    }

    #[test]
    fn epub3_property_meta_is_not_an_epub2_cover_declaration() {
        // EPUB3 的 `<meta property="...">` 与 EPUB2 的 `<meta name="...">` 是两种写法
        let package = cover_package(
            r#"<meta property="cover">cov</meta>
               <meta property="dcterms:modified">2024-01-01T00:00:00Z</meta>"#,
            r#"<item id="cov" href="images/cover.jpg" media-type="image/jpeg"/>"#,
        );
        assert!(
            package.metadata.cover_id.is_none(),
            "property 不能被当成 name"
        );
        assert_eq!(package.cover_image(), None);
    }

    #[test]
    fn epub2_cover_pointing_at_a_cover_page_is_rejected() {
        // 少数 EPUB2 把 name="cover" 指向 XHTML 封面**页**而不是图片。
        // 那个 href 不能当缩略图用（会渲染成文字/坏图），必须拒绝。
        let package = cover_package(
            r#"<meta name="cover" content="coverpage"/>"#,
            r#"<item id="coverpage" href="cover.xhtml" media-type="application/xhtml+xml"/>"#,
        );
        assert_eq!(package.metadata.cover_id.as_deref(), Some("coverpage"));
        assert_eq!(package.cover_image(), None, "指向封面页时不能当图片返回");
    }

    #[test]
    fn epub2_cover_without_media_type_is_accepted() {
        // media-type 缺失时无从判断，放行而不是一律拒绝
        let package = cover_package(
            r#"<meta name="cover" content="cov"/>"#,
            r#"<item id="cov" href="images/cover.jpg"/>"#,
        );
        assert_eq!(package.cover_image(), Some("/OEBPS/images/cover.jpg"));
    }

    #[test]
    fn epub2_cover_meta_value_is_case_tolerant_and_first_wins() {
        // 属性值放宽为 ASCII 大小写不敏感（实践中见过 name="Cover"）；
        // 多份声明与其它单值元数据字段一致：第一份生效。
        let package = cover_package(
            r#"<meta name="Cover" content="cov"/>
               <meta name="cover" content="other"/>"#,
            r#"<item id="cov" href="images/cover.jpg" media-type="image/jpeg"/>
               <item id="other" href="images/other.jpg" media-type="image/jpeg"/>"#,
        );
        assert_eq!(package.metadata.cover_id.as_deref(), Some("cov"));
        assert_eq!(package.cover_image(), Some("/OEBPS/images/cover.jpg"));
    }
}
