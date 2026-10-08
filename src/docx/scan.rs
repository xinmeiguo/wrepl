//! `<w:t>` 字节偏移扫描 + 跨 run 段落重组（里程碑 M2）。
//!
//! ## 为什么必须做这一层
//!
//! Word 会因为拼写检查、局部加粗、修订等原因，把**一句话切成好几个 run**：
//!
//! ```xml
//! <w:p>
//!   <w:r><w:t>某某生物</w:t></w:r>
//!   <w:r><w:rPr><w:b/></w:rPr><w:t>制品有限公司</w:t></w:r>
//! </w:p>
//! ```
//!
//! 这时直接按 `<w:t>` 逐个做字符串查找，`某某生物制品有限公司` **一个都匹配不到**——
//! 替换会静默失效。所以必须先把段落内的所有文本节点拼成一条"可见字符串"，
//! 再把每个字符映射回它在**原始字节流中的位置**，这样才有跨 run 替换的可能。
//!
//! ## 为什么不建模重建
//!
//! 本模块只做**流式扫描 + 记录偏移**，绝不"解析成树再序列化回去"。
//! 序列化会把属性顺序、命名空间前缀、缩进、以及解析器不认识的内容全部改写；
//! 记录偏移则可以让后续替换**直接在原始字节上定点编辑**，其余字节原样不动。
//!
//! ## 坐标约定
//!
//! 所有偏移都是**相对该 part 自身 XML 字节流**的绝对字节位置。
//! 不同 part 的坐标系彼此独立，不可跨 part 比较（这也是冲突检测的基本单位）。
//!
//! ## 三条"少一条就出错"的判定规则
//!
//! 1. **元素必须属于 WordprocessingML 命名空间。**
//!    DrawingML 里也有 `a:p`（图形段落）、`a:r`、`a:t`，局部名一模一样。
//!    只按 local name 判断，形状 / SmartArt 里的文字会被误当正文段落——
//!    于是凭空打开一个假 `w:p`，后面的偏移与匹配全错。
//! 2. **文本承载元素必须是 `<w:r>` 的直接子元素。**
//!    否则 `<w:pPr><w:tabs><w:tab/></w:tabs>` 里的**制表位定义**会被误当成正文里的制表符，
//!    凭空多出一个可见字符。`w:r` 同样必须是 W 命名空间的那个。
//! 3. **段落必须按嵌套深度各自成栈。**
//!    文本框（`w:pict` / `wps:txbx`）通常挂在某个 run 里，于是出现
//!    `<w:p>…<w:r><w:pict>…<w:txbxContent><w:p>…</w:p></w:txbxContent>…</w:p>` 这种嵌套。
//!    用单个"当前段落"变量会导致：内层段落结束时把外层段落提前收尾，
//!    外层后续 run 的文本全部丢失；且外层段落被错误地判定为"非文本框"。

use anyhow::{Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use std::collections::HashMap;

/// WordprocessingML 主命名空间——**只有属于它的 `p` / `r` / `t` 才是我们关心的元素**。
const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

/// 段落内一个文本承载节点的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// `<w:t>` —— 正常可见文本
    Text,
    /// `<w:delText>` —— 修订模式下**已被删除**的内容，Word 中不可见。
    /// 改它没有意义，且可能破坏修订记录，因此不计入可见文本。
    DelText,
    /// `<w:instrText>` —— 域代码（目录、页码、交叉引用）。改坏了域直接失效。
    InstrText,
    /// `<w:noBreakHyphen/>` —— **空元素，不含文本节点**，但显示为一个 `-`。
    /// 不做映射的话，`DP-AUC-01` 会被匹配成 `DPAUC01`。
    NoBreakHyphen,
    /// `<w:tab/>` —— 制表符
    Tab,
    /// `<w:br/>` / `<w:cr/>` —— 手动换行（软回车）
    Break,
    /// `<w:softHyphen/>` —— 可选连字符
    SoftHyphen,
}

impl NodeKind {
    pub fn label(self) -> &'static str {
        match self {
            NodeKind::Text => "<w:t>",
            NodeKind::DelText => "<w:delText>",
            NodeKind::InstrText => "<w:instrText>",
            NodeKind::NoBreakHyphen => "<w:noBreakHyphen/>",
            NodeKind::Tab => "<w:tab/>",
            NodeKind::Break => "<w:br/>",
            NodeKind::SoftHyphen => "<w:softHyphen/>",
        }
    }

    /// 是否计入**可见文本**（决定它会不会参与查找）。
    pub fn is_visible(self) -> bool {
        !matches!(self, NodeKind::DelText | NodeKind::InstrText)
    }

    /// 是否是空元素——即元素本身**不含文本节点**，却要占用一个可见字符位。
    pub fn is_empty_elem(self) -> bool {
        matches!(
            self,
            NodeKind::NoBreakHyphen | NodeKind::Tab | NodeKind::Break | NodeKind::SoftHyphen
        )
    }

    /// 空元素对应的虚拟字符（无字节范围的字符）。
    pub fn virtual_char(self) -> Option<char> {
        match self {
            NodeKind::NoBreakHyphen => Some('-'),
            NodeKind::Tab => Some('\t'),
            NodeKind::Break => Some('\n'),
            NodeKind::SoftHyphen => Some('\u{00AD}'),
            _ => None,
        }
    }

    /// XML 中的元素 local name。
    fn local_name(self) -> &'static str {
        match self {
            NodeKind::Text => "t",
            NodeKind::DelText => "delText",
            NodeKind::InstrText => "instrText",
            NodeKind::NoBreakHyphen => "noBreakHyphen",
            NodeKind::Tab => "tab",
            NodeKind::Break => "br",
            NodeKind::SoftHyphen => "softHyphen",
        }
    }

    /// 由元素 local name 反查节点种类（**调用方必须先确认命名空间**）。
    fn from_local_name(name: &str) -> Option<Self> {
        match name {
            "t" => Some(NodeKind::Text),
            "delText" => Some(NodeKind::DelText),
            "instrText" => Some(NodeKind::InstrText),
            "noBreakHyphen" => Some(NodeKind::NoBreakHyphen),
            "tab" => Some(NodeKind::Tab),
            "br" | "cr" => Some(NodeKind::Break),
            "softHyphen" => Some(NodeKind::SoftHyphen),
            _ => None,
        }
    }
}

/// 段落内一个文本承载节点，带它在原始字节流中的精确位置。
#[derive(Debug, Clone)]
pub struct TextNode {
    pub kind: NodeKind,
    /// 元素自身起始（`<` 的位置）
    pub elem_start: usize,
    /// 元素结束（`>` 之后的位置）
    pub elem_end: usize,
    /// 文本内容区间：**空元素时该区间为空**（这也是"空元素不含文本节点"的体现）
    pub content_start: usize,
    pub content_end: usize,
    /// 原始文本内容（**转义态**，与字节流一一对应）
    pub raw: String,
    /// 所在 `<w:t>` 是否带 `xml:space="preserve"`
    pub preserve_space: bool,
    /// 所在 run 是否有 `<w:rPr>`（决定替换时格式继承的落点）
    pub run_has_rpr: bool,
    /// 段内第几个 run
    pub run_index: usize,
}

impl TextNode {
    /// 内容字节区间（用于后续定点替换）。
    pub fn byte_range(&self) -> std::ops::Range<usize> {
        self.content_start..self.content_end
    }

    /// 该节点内容是否被 `<![CDATA[` 包裹——决定写入时是否需要转义。
    pub fn is_cdata(&self, xml: &[u8]) -> bool {
        const OPEN: &[u8] = b"<![CDATA[";
        self.content_start >= OPEN.len() && &xml[self.content_start - OPEN.len()..self.content_start] == OPEN
    }
}

/// 可见文本中的**一个字符**到原始字节的映射。
#[derive(Debug, Clone, Copy)]
pub struct CharRef {
    /// 落在第几个 node 上
    pub node: usize,
    /// 该字符在原始字节流中的起点
    pub off: usize,
    /// 该字符占用的字节数；**0 表示虚拟字符**（来自空元素，没有可编辑的文本字节）
    pub len: usize,
}

impl CharRef {
    /// 是否为虚拟字符（来自 `noBreakHyphen` / `tab` / `br` 这类空元素）。
    pub fn is_virtual(&self) -> bool {
        self.len == 0
    }
}

/// 一个段落（`<w:p>`）及其内部文本结构。
#[derive(Debug, Clone)]
pub struct Para {
    /// 在本 part 内的段落序号（从 0 开始，按**闭合顺序**）
    pub index: usize,
    /// 所属 part 名，如 `word/document.xml`
    pub part: String,
    /// `<w:p>` 元素起止
    pub elem_start: usize,
    pub elem_end: usize,
    /// 是否位于文本框内（`w:txbxContent` 子树）——文本框与正文**不是同一棵树**
    pub in_textbox: bool,
    /// 段内全部文本承载节点，按文档顺序
    pub nodes: Vec<TextNode>,
    /// 可见文本（已解码实体、已把空元素映射为对应字符）
    pub visible: String,
    /// 与 `visible` 的每个字符一一对应
    pub map: Vec<CharRef>,
    /// 段内 run 总数
    pub run_count: usize,
}

impl Para {
    /// 可见文本被切分到了几个 `<w:t>` 上。`>= 2` 即"跨 run"，直接搜单个 `<w:t>` 会漏。
    pub fn visible_text_nodes(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Text && !n.raw.is_empty())
            .count()
    }

    /// 是否跨 run。
    pub fn is_split_across_runs(&self) -> bool {
        self.visible_text_nodes() >= 2
    }

    /// 取可见字符区间 `[a, b)` 对应的原始字节区间。
    ///
    /// 若区间首尾触及**虚拟字符**，返回 `None`——因为虚拟字符没有自己的文本字节，
    /// 无法用"删一段字节"来表达移除它（需要显式处理空元素，见 `rewrite` 模块）。
    pub fn byte_span(&self, a: usize, b: usize) -> Option<std::ops::Range<usize>> {
        if a >= b || b > self.map.len() {
            return None;
        }
        let first = self.map[a];
        let last = self.map[b - 1];
        if first.is_virtual() && last.is_virtual() {
            return None;
        }
        let start = first.off;
        let end = last.off + last.len;
        Some(start..end)
    }

    /// 取可见字符区间 `[a, b)` 覆盖到的 node 下标（按出现顺序，可能不连续——
    /// 中间夹着的 `delText` / `instrText` 不含可见字符，不会被覆盖）。
    pub fn covered_nodes(&self, a: usize, b: usize) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        let lo = a.min(self.map.len());
        let hi = b.min(self.map.len());
        for r in &self.map[lo..hi] {
            if out.last() != Some(&r.node) {
                out.push(r.node);
            }
        }
        out
    }

    /// 取该 node 在可见字符区间 `[a, b)` 内被覆盖到的字节区间（仅对含文本节点的 node 有意义）。
    pub fn node_covered_range(
        &self,
        node: usize,
        a: usize,
        b: usize,
    ) -> Option<std::ops::Range<usize>> {
        let mut lo: Option<usize> = None;
        let mut hi: Option<usize> = None;
        for (i, r) in self.map.iter().enumerate() {
            if i < a || i >= b || r.node != node || r.is_virtual() {
                continue;
            }
            lo = Some(match lo {
                Some(v) => v.min(r.off),
                None => r.off,
            });
            hi = Some(match hi {
                Some(v) => v.max(r.off + r.len),
                None => r.off + r.len,
            });
        }
        match (lo, hi) {
            (Some(l), Some(h)) if h > l => Some(l..h),
            _ => None,
        }
    }
}

/// 扫描结果统计，用于 M2 的人工核对与后续里程碑的输入画像。
#[derive(Debug, Default, Clone)]
pub struct ScanStats {
    pub paragraphs: usize,
    pub visible_chars: usize,
    pub text_nodes: usize,
    pub del_text_nodes: usize,
    pub instr_text_nodes: usize,
    pub empty_elem_nodes: usize,
    pub split_paragraphs: usize,
    pub textbox_paragraphs: usize,
    pub preserve_space_nodes: usize,
    pub runs: usize,
}

impl ScanStats {
    pub fn merge(&mut self, other: &ScanStats) {
        self.paragraphs += other.paragraphs;
        self.visible_chars += other.visible_chars;
        self.text_nodes += other.text_nodes;
        self.del_text_nodes += other.del_text_nodes;
        self.instr_text_nodes += other.instr_text_nodes;
        self.empty_elem_nodes += other.empty_elem_nodes;
        self.split_paragraphs += other.split_paragraphs;
        self.textbox_paragraphs += other.textbox_paragraphs;
        self.preserve_space_nodes += other.preserve_space_nodes;
        self.runs += other.runs;
    }
}

/// 正在收集中的段落（含它自己的 run 状态，因为段落可以嵌套）。
struct OpenPara {
    para: Para,
    run_index: usize,
    run_has_rpr: bool,
}

/// 正在累积中的文本承载元素（`<w:t>` / `<w:delText>` / `<w:instrText>`）。
///
/// **为什么不在这元素的第一个 `Event::Text` 就把节点定下来**：
/// quick-xml 把 `&#nnn;`（字符引用）和 `&amp;`（实体引用）单独发成
/// `Event::GeneralRef`，于是 `<w:t>某某</w:t>` 若写成
/// `<w:t>&#36149;&#24030;</w:t>`，事件序列是 `Start, GeneralRef, GeneralRef, End`——
/// 一个文本事件都没有。若在首个文本事件就定型，引用事件无处可去，
/// **文本会被静默丢掉**（实测可见字符 185 → 183，且不报任何错）。
/// 所以这里先累积，等元素闭合再一次性定型。
struct PendingText {
    kind: NodeKind,
    /// 元素自身 `<` 的位置
    elem_start: usize,
    /// `<w:t>` 上的 `xml:space="preserve"`
    preserve_space: bool,
    /// 累积的**转义态**文本。引用按原文 `&名字;` 原样拼回，
    /// 因此它与「元素内容区间」字节一一对应，可直接交给 `decode_with_offsets`。
    raw: String,
    /// 内容起点；`None` = 至今没有任何文本/引用事件（即"开了没内容"）
    content_start: Option<usize>,
    /// 最后一个文本/引用事件结束后的字节位置
    last_end: usize,
}

/// 扫描状态。把命名空间表、元素栈、段落栈放在一起，避免逐个当参数传递。
struct Ctx {
    base_index: usize,
    stack: Vec<(String, bool)>,
    ns_prefix: HashMap<String, String>,
    ns_default: String,
    open: Vec<OpenPara>,
    pending: Option<PendingText>,
}

impl Ctx {
    fn new(base_index: usize) -> Self {
        Ctx {
            base_index,
            stack: Vec::new(),
            ns_prefix: HashMap::new(),
            ns_default: String::new(),
            open: Vec::new(),
            pending: None,
        }
    }

    /// 前缀 → URI；空前缀取默认命名空间。
    fn uri_of(&self, prefix: &str) -> Option<&str> {
        if prefix.is_empty() {
            if self.ns_default.is_empty() {
                None
            } else {
                Some(self.ns_default.as_str())
            }
        } else {
            self.ns_prefix.get(prefix).map(|s| s.as_str())
        }
    }

    /// 当前父元素是否就是 W 命名空间的 `<w:r>`——文本承载元素的唯一合法父节点。
    fn parent_is_run(&self) -> bool {
        match self.stack.last() {
            Some((n, true)) => n == "r",
            _ => false,
        }
    }

    fn run_index(&self) -> usize {
        self.open.last().map(|o| o.run_index).unwrap_or(0)
    }

    fn run_has_rpr(&self) -> bool {
        self.open.last().map(|o| o.run_has_rpr).unwrap_or(false)
    }

    fn push_node(&mut self, node: TextNode) {
        if let Some(op) = self.open.last_mut() {
            op.para.nodes.push(node);
        }
    }

    fn push_empty_node(&mut self, kind: NodeKind, elem_start: usize, elem_end: usize) {
        let (run_index, run_has_rpr) = (self.run_index(), self.run_has_rpr());
        self.push_node(TextNode {
            kind,
            elem_start,
            elem_end,
            content_start: elem_end,
            content_end: elem_end,
            raw: String::new(),
            preserve_space: false,
            run_has_rpr,
            run_index,
        });
    }

    /// 处理一个 Start / Empty 元素。
    fn on_element(&mut self, e: &BytesStart<'_>, empty_tag: bool, before: usize, after: usize) {
        // 0. 防御：挂着的文本元素遇到新元素就收尾（正常不会走到，
        //    因为文本元素里不会再有子元素）
        if self.pending.is_some() {
            self.flush_pending(before);
        }

        // 1. 先收集命名空间声明（声明可能就写在这个元素自己身上）
        for attr in e.attributes().flatten() {
            let key: &str = attr.key.as_ref();
            let val: &str = attr.value.as_ref();
            if let Some(p) = key.strip_prefix("xmlns:") {
                self.ns_prefix.insert(p.to_string(), val.to_string());
            } else if key == "xmlns" {
                self.ns_default = val.to_string();
            }
        }

        // 2. 解析限定名，判断是否属于 W 命名空间
        let raw_name = e.name();
        let qname: &str = raw_name.as_ref();
        let (prefix, local) = match qname.split_once(':') {
            Some((p, l)) => (p, l),
            None => ("", qname),
        };
        let is_wml = self.uri_of(prefix) == Some(W_NS);

        // 3. 分派
        //
        // ⚠ 结构性元素只在 `Event::Start` 时处理：
        // `<w:p/>` 这种自闭合空段落没有配对的 End 事件，若在这里"打开"段落，
        // 它会永远挂在段落栈上，直到文档末尾才被兜底收尾——
        // 于是冒出一个字节区间错乱、run 数为 0 的幽灵段落。
        // `<w:r/>` 同理（会凭空多算一个 run）。
        if is_wml {
            match local {
                "p" if !empty_tag => {
                    let index = self.base_index + self.open.len();
                    let in_textbox = self
                        .stack
                        .iter()
                        .any(|(n, w)| *w && n == "txbxContent");
                    self.open.push(OpenPara {
                        para: Para {
                            index,
                            part: String::new(), // 由调用方在 finalize 前补上
                            elem_start: before,
                            elem_end: after,
                            in_textbox,
                            nodes: Vec::new(),
                            visible: String::new(),
                            map: Vec::new(),
                            run_count: 0,
                        },
                        run_index: 0,
                        run_has_rpr: false,
                    });
                }
                "r" if !empty_tag => {
                    if let Some(op) = self.open.last_mut() {
                        op.run_index = op.para.run_count;
                        op.run_has_rpr = false;
                        op.para.run_count += 1;
                    }
                }
                "rPr" if !empty_tag => {
                    // 只有直接挂在 run 下的 rPr 才是"文字属性"；
                    // <w:pPr><w:rPr> 属于段落标记属性，不算。
                    if self.parent_is_run() {
                        if let Some(op) = self.open.last_mut() {
                            op.run_has_rpr = true;
                            let ri = op.run_index;
                            for n in op.para.nodes.iter_mut().rev() {
                                if n.run_index == ri {
                                    n.run_has_rpr = true;
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                }
                _ => {
                    if self.parent_is_run() {
                        if let Some(kind) = NodeKind::from_local_name(local) {
                            if kind.is_empty_elem() {
                                // 含罕见的 <w:tab></w:tab> 写法——依然按空元素记
                                self.push_empty_node(kind, before, after);
                            } else if empty_tag {
                                // <w:t/> 自闭合：元素存在但内容为空
                                self.push_node(TextNode {
                                    kind,
                                    elem_start: before,
                                    elem_end: after,
                                    content_start: before,
                                    content_end: before,
                                    raw: String::new(),
                                    preserve_space: has_preserve_space(e),
                                    run_has_rpr: self.run_has_rpr(),
                                    run_index: self.run_index(),
                                });
                            } else {
                                let preserve = has_preserve_space(e);
                                self.pending = Some(PendingText {
                                    kind,
                                    elem_start: before,
                                    preserve_space: preserve,
                                    raw: String::new(),
                                    content_start: None,
                                    last_end: before,
                                });
                            }
                        }
                    }
                }
            }
        }

        if !empty_tag {
            self.stack.push((local.to_string(), is_wml));
        }
    }

    /// 处理文本 / CDATA 事件：累积进当前挂着的文本元素。
    fn on_text(&mut self, raw: String, before: usize, after: usize) {
        if let Some(p) = self.pending.as_mut() {
            if p.content_start.is_none() {
                p.content_start = Some(before);
            }
            p.raw.push_str(&raw);
            p.last_end = after;
        }
        // pending 为 None 时的文本事件是元素间空白，忽略
    }

    /// 处理引用事件（`&#nnn;` / `&amp;` 等）：按原始写法拼回转义态缓冲。
    ///
    /// 引用在原文里就是 `&` + 名字 + `;`，原样拼回才能与
    /// `decode_with_offsets`「按字节切片再解码」的假设保持一致——
    /// 拼回去的字节数与原文字节数完全相同，偏移不会漂。
    fn on_ref(&mut self, inner: &str, before: usize, after: usize) {
        if let Some(p) = self.pending.as_mut() {
            if p.content_start.is_none() {
                p.content_start = Some(before);
            }
            p.raw.push('&');
            p.raw.push_str(inner);
            p.raw.push(';');
            p.last_end = after;
        }
    }

    /// 把挂着的文本元素定型成一个节点。`end_tag_at` 是闭合标签 `<` 的位置。
    fn flush_pending(&mut self, end_tag_at: usize) {
        let Some(p) = self.pending.take() else {
            return;
        };
        let (content_start, content_end) = match p.content_start {
            Some(cs) => (cs, p.last_end.max(cs)),
            // 「开了没内容」：`<w:t></w:t>` —— 内容区间为空
            None => (end_tag_at, end_tag_at),
        };
        let (run_index, run_has_rpr) = (self.run_index(), self.run_has_rpr());
        self.push_node(TextNode {
            kind: p.kind,
            elem_start: p.elem_start,
            elem_end: end_tag_at,
            content_start,
            content_end,
            raw: p.raw,
            preserve_space: p.preserve_space,
            run_has_rpr,
            run_index,
        });
    }

    /// 处理 End 元素；返回刚闭合的段落（若有）。
    fn on_end(&mut self, local: &str, before: usize, after: usize) -> Option<Para> {
        // 挂着的文本元素在此收尾（含 `<w:t></w:t>` 这种"开了没内容"的写法）
        let matches = self
            .pending
            .as_ref()
            .is_some_and(|p| p.kind.local_name() == local);
        if matches {
            self.flush_pending(before);
        }

        let was_wml = match self.stack.pop() {
            Some((_, w)) => w,
            None => false,
        };

        if local == "p" && was_wml {
            if let Some(mut op) = self.open.pop() {
                op.para.elem_end = after;
                return Some(op.para);
            }
        }
        None
    }
}

/// 扫描一个 part，按**段落闭合顺序**返回全部段落。
pub fn scan_part(part: &str, xml: &str) -> Result<(Vec<Para>, ScanStats)> {
    let mut reader = Reader::from_str(xml);
    // 少数真实文档存在标签不闭合的瑕疵；我们要的是偏移，不是严格的良构校验。
    // 关掉它，避免因为一个无关的闭合问题整篇扫不动。
    reader.config_mut().check_end_names = false;

    let mut ctx = Ctx::new(0);
    let mut paras: Vec<Para> = Vec::new();
    let mut stats = ScanStats::default();

    loop {
        let before = reader.buffer_position() as usize;
        let ev = reader
            .read_event()
            .with_context(|| format!("{part} XML 解析失败（字节偏移 {before}）"))?;
        let after = reader.buffer_position() as usize;

        match ev {
            Event::Eof => break,
            Event::Start(e) => ctx.on_element(&e, false, before, after),
            Event::Empty(e) => ctx.on_element(&e, true, before, after),
            Event::Text(e) => ctx.on_text(e.into_inner().into_owned(), before, after),
            Event::CData(e) => ctx.on_text(e.into_inner().into_owned(), before, after),
            // `&#nnn;` / `&amp;`：quick-xml 单独发的事件，必须显式接住，
            // 否则这些字符会被静默丢弃（见 PendingText 的说明）。
            Event::GeneralRef(e) => {
                let inner = e.into_inner().into_owned();
                ctx.on_ref(&inner, before, after);
            }
            Event::End(e) => {
                let local = e.local_name().as_ref().to_string();
                if let Some(mut p) = ctx.on_end(&local, before, after) {
                    p.part = part.to_string();
                    p.index = paras.len();
                    finalize_para(&mut p, &mut stats);
                    paras.push(p);
                }
            }
            _ => {}
        }
    }

    // 文档结尾仍挂着的文本元素（标签不闭合的瑕疵文档）：兜底收尾，别丢文本
    ctx.flush_pending(xml.len());

    // 文档被截断在段落中间时，把还没收尾的段落也交出去，避免静默丢文本
    while let Some(op) = ctx.open.pop() {
        let mut p = op.para;
        p.part = part.to_string();
        p.index = paras.len();
        finalize_para(&mut p, &mut stats);
        paras.push(p);
    }

    Ok((paras, stats))
}

/// 组装可见文本与字符→字节映射，并累计统计。
fn finalize_para(p: &mut Para, stats: &mut ScanStats) {
    let mut visible = String::new();
    let mut map: Vec<CharRef> = Vec::new();

    stats.paragraphs += 1;
    stats.runs += p.run_count;
    if p.in_textbox {
        stats.textbox_paragraphs += 1;
    }

    for (ni, node) in p.nodes.iter().enumerate() {
        match node.kind {
            NodeKind::Text => {
                stats.text_nodes += 1;
                if node.preserve_space {
                    stats.preserve_space_nodes += 1;
                }
                for (ch, off, len) in decode_with_offsets(&node.raw, node.content_start) {
                    visible.push(ch);
                    map.push(CharRef {
                        node: ni,
                        off,
                        len,
                    });
                }
            }
            NodeKind::DelText => stats.del_text_nodes += 1,
            NodeKind::InstrText => stats.instr_text_nodes += 1,
            _ => {
                stats.empty_elem_nodes += 1;
                if let Some(ch) = node.kind.virtual_char() {
                    visible.push(ch);
                    map.push(CharRef {
                        node: ni,
                        off: node.elem_start,
                        len: 0, // 虚拟字符：没有可编辑的文本字节
                    });
                }
            }
        }
    }

    if p.is_split_across_runs() {
        stats.split_paragraphs += 1;
    }
    stats.visible_chars += visible.chars().count();

    p.visible = visible;
    p.map = map;
}

/// 判断 `<w:t>` 是否带 `xml:space="preserve"`。
///
/// 漏掉这个属性，首尾带空格的替换串在 Word 里会被吞掉空格——文本"悄悄变了"。
fn has_preserve_space(e: &BytesStart<'_>) -> bool {
    for attr in e.attributes().flatten() {
        let key: &str = attr.key.as_ref();
        let val: &str = attr.value.as_ref();
        if key == "xml:space" {
            return val == "preserve";
        }
    }
    false
}

/// 把原始（转义态）文本解码成 `(字符, 字节起点, 字节长度)`。
///
/// 自己实现而不用库的解码函数，因为**必须同时拿到字节长度**：
/// 库的 `xml_content()` 会做 EOL 规范化，长度对不上原文，偏移就废了。
fn decode_with_offsets(raw: &str, base: usize) -> Vec<(char, usize, usize)> {
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(semi) = raw[i..].find(';') {
                if semi <= 12 {
                    let ent = &raw[i + 1..i + semi];
                    if let Some(ch) = decode_entity(ent) {
                        out.push((ch, base + i, semi + 1));
                        i += semi + 1;
                        continue;
                    }
                }
            }
        }
        // 普通 UTF-8 字符
        let ch = raw[i..].chars().next().expect("i 恒在字符边界上");
        let len = ch.len_utf8();
        out.push((ch, base + i, len));
        i += len;
    }

    out
}

fn decode_entity(ent: &str) -> Option<char> {
    match ent {
        "amp" => return Some('&'),
        "lt" => return Some('<'),
        "gt" => return Some('>'),
        "quot" => return Some('"'),
        "apos" => return Some('\''),
        _ => {}
    }
    let n = ent.strip_prefix('#')?;
    let cp = if let Some(h) = n.strip_prefix('x').or_else(|| n.strip_prefix('X')) {
        u32::from_str_radix(h, 16).ok()?
    } else {
        n.parse::<u32>().ok()?
    };
    char::from_u32(cp)
}

/// XML 1.0 **不允许出现**的字符（写进文档会产出非法 XML）。
///
/// 判据取自 XML 1.0 的 `Char` 产生式：`#x9 | #xA | #xD | [#x20-#xD7FF] | …`。
/// 也就是 0x00–0x08、0x0B、0x0C、0x0E–0x1F 全在禁止之列，外加两个非字符
/// U+FFFE / U+FFFF。**制表、换行、回车是合法的**，不在这里面。
///
/// 这类字符可能从规则文件/Excel 单元格里混进来（从别处复制粘贴最容易带上）。
/// 一旦原样写进 `<w:t>`，Word 打开时会报"发现不可读取的内容"，整份文档打不开——
/// 所以必须在**写入**这一步挡掉，代价是静默丢弃（这类字符本来就没有可见含义）。
fn is_xml_illegal(c: char) -> bool {
    matches!(
        c,
        '\u{0000}'..='\u{0008}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000E}'..='\u{001F}'
            | '\u{FFFE}'
            | '\u{FFFF}'
    )
}

/// 把带 `&<>` 等字符的普通文本转成 XML 转义态（写入时用）。
pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // XML 非法控制字符直接丢掉：留着会写出非法 XML，整份文档都读不开。
            c if is_xml_illegal(c) => {}
            _ => out.push(ch),
        }
    }
    out
}

/// 替换串首尾是否带空白——带的话必须确保目标 `<w:t>` 上有 `xml:space="preserve"`。
pub fn needs_preserve(s: &str) -> bool {
    s.starts_with(|c: char| c.is_whitespace()) || s.ends_with(|c: char| c.is_whitespace())
}

/// 段落可见文本的可读化（制表符/换行符转可见记号，便于报告核对）。
pub fn display_visible(s: &str) -> String {
    s.replace('\t', "→")
        .replace('\n', "⏎")
        .replace('\u{00AD}', "·")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_text_escapes_xml_markup() {
        assert_eq!(escape_text("a<b>&c"), "a&lt;b&gt;&amp;c");
        assert_eq!(escape_text("原样保留"), "原样保留");
    }

    #[test]
    fn escape_text_drops_xml_illegal_control_chars() {
        // XML 1.0 禁止 0x00–0x08 / 0x0B / 0x0C / 0x0E–0x1F。这类字符多是从别处
        // 复制粘贴带进规则表的；一旦原样写进 `<w:t>`，Word 打开会报"发现不可读取的
        // 内容"，整份文档打不开——所以必须在写入这一步丢掉。
        assert_eq!(
            escape_text("a\u{0}b\u{1}c\u{8}d\u{b}e\u{c}f\u{e}g\u{1f}h"),
            "abcdefgh"
        );
        // 非字符 U+FFFE / U+FFFF 同样非法
        assert_eq!(escape_text("x\u{FFFE}y\u{FFFF}z"), "xyz");
    }

    #[test]
    fn escape_text_keeps_legal_whitespace() {
        // 制表 / 换行 / 回车是 XML 合法字符，必须原样保留（丢了会改掉正文排版）
        assert_eq!(escape_text("a\tb\nc\rd"), "a\tb\nc\rd");
        assert!(is_xml_illegal('\u{0B}'));
        assert!(!is_xml_illegal('\t'));
        assert!(!is_xml_illegal(' '));
        assert!(!is_xml_illegal('中'));
    }
}
