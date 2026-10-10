//! 定点字节编辑：把匹配到的内容在**原始字节流**上改掉，其余字节一个不碰。
//!
//! ## 铁律：跨 run 命中不能整段覆盖
//!
//! 一段跨 run 的文本，其"最小外包字节区间"中间夹着
//! `</w:t></w:r><w:r><w:rPr>…` 这类标记。**整段覆盖 = 把中间 run 的格式一起删掉。**
//!
//! 所以命中区间必须**按节点分发**：
//!
//! | 命中覆盖到的节点 | 处理 |
//! |---|---|
//! | 第一个含文本的节点 | 替换其被覆盖的那段字节（替换串落在这里，继承它的 `<w:rPr>`） |
//! | 其余含文本的节点 | **只清空被覆盖的文本字节**，`<w:r>` 与 `<w:rPr>` 元素原样保留 |
//! | 被覆盖的空元素（`noBreakHyphen` / `tab` / `br`） | 删除元素本身——这是唯一能"去掉"它的办法 |
//! | 整段命中全是空元素 | 删掉它们，在原位插入一个新的 `<w:t>`（属性继承所在 run） |
//!
//! ## 另外两件必须做对的小事
//!
//! - **`xml:space="preserve"`**：替换串首尾带空格而目标 `<w:t>` 没这个属性时，Word 会把空格吞掉。
//! - **CDATA**：`<![CDATA[…]]>` 里的内容不能写实体转义，否则会变成字面 `&amp;`。

use crate::docx::scan::{self, Para};
use anyhow::{bail, Result};

/// 一次字节级编辑：把 `[at, at + del)` 换成 `ins`。`del == 0` 即纯插入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub at: usize,
    pub del: usize,
    pub ins: Vec<u8>,
}

/// 本次命中的改写方式（进报告，便于人工核对）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// 替换串写在命中的第一个文本节点里（最常见）
    InPlaceText,
    /// 命中全是空元素，需新插入一个 `<w:t>`
    NewTextElement,
    /// 纯删除（替换串为空），无新增元素
    DeleteOnly,
}

impl Strategy {
    pub fn label(self) -> &'static str {
        match self {
            Strategy::InPlaceText => "就地替换文本节点",
            Strategy::NewTextElement => "新增 <w:t> 承载替换串",
            Strategy::DeleteOnly => "纯删除",
        }
    }
}

/// 一次命中的编辑计划。
#[derive(Debug, Clone)]
pub struct HitEdits {
    pub edits: Vec<Edit>,
    pub strategy: Strategy,
    /// 被删掉的 run 内空元素个数（`noBreakHyphen` / `tab` / `br` / `softHyphen`）
    pub removed_empty: usize,
    /// 是否新增了一个 `<w:t>`（仅"命中全是空元素"时发生）
    pub added_text_elem: bool,
}

impl HitEdits {
    /// 本次改写的完整描述——**把结构性手术讲清楚**，便于人工与关卡 2 对照。
    pub fn describe(&self) -> String {
        let mut s = self.strategy.label().to_string();
        if self.removed_empty > 0 {
            s.push_str(&format!("；删除空元素 ×{}", self.removed_empty));
        }
        if self.added_text_elem {
            s.push_str("；新增 <w:t> ×1");
        }
        s
    }
}

/// 为一次命中（可见字符区间 `[a, b)`）生成编辑计划。
pub fn build(xml: &[u8], para: &Para, a: usize, b: usize, replacement: &str) -> Result<HitEdits> {
    if a >= b || b > para.map.len() {
        bail!("命中区间 [{}..{}) 非法", a, b);
    }

    let covered = para.covered_nodes(a, b);
    if covered.is_empty() {
        bail!("命中区间 [{}..{}) 未覆盖任何节点", a, b);
    }

    let mut edits: Vec<Edit> = Vec::new();
    // 替换串的落点：命中里第一个"含文本节点"的节点
    let mut anchor: Option<(usize, std::ops::Range<usize>)> = None;
    let mut removed_empty = 0usize;

    for &ni in &covered {
        let node = &para.nodes[ni];
        if node.kind.is_empty_elem() {
            // 空元素：整块删掉（不移除所在 run，格式不受影响）
            edits.push(Edit {
                at: node.elem_start,
                del: node.elem_end - node.elem_start,
                ins: Vec::new(),
            });
            removed_empty += 1;
        } else {
            let r = match para.node_covered_range(ni, a, b) {
                Some(r) => r,
                None => bail!("命中区间 [{}..{}) 在节点 #{ni} 上取不到字节范围", a, b),
            };
            if anchor.is_none() {
                anchor = Some((ni, r));
            } else {
                // 其余文本节点：只清空被覆盖的字节，元素与 rPr 保留
                edits.push(Edit {
                    at: r.start,
                    del: r.end - r.start,
                    ins: Vec::new(),
                });
            }
        }
    }

    let strategy = match anchor {
        Some((ni, r)) => {
            let node = &para.nodes[ni];
            let ins = encode(xml, node, replacement)?;
            edits.push(Edit {
                at: r.start,
                del: r.end - r.start,
                ins,
            });

            // 首尾带空格的替换串必须落在 preserve 属性下，否则 Word 会吞掉空格
            if !replacement.is_empty()
                && scan::needs_preserve(replacement)
                && !node.preserve_space
            {
                let at = attr_insert_pos(xml, node)?;
                edits.push(Edit {
                    at,
                    del: 0,
                    ins: b" xml:space=\"preserve\"".to_vec(),
                });
            }
            Strategy::InPlaceText
        }
        None => {
            // 命中的全是空元素：删掉它们，在原位插入一个新的 <w:t>
            if replacement.is_empty() {
                Strategy::DeleteOnly
            } else {
                let last = *covered.last().expect("covered 非空");
                let at = para.nodes[last].elem_end;
                let ins = format!(
                    "<w:t xml:space=\"preserve\">{}</w:t>",
                    scan::escape_text(replacement)
                )
                .into_bytes();
                edits.push(Edit { at, del: 0, ins });
                Strategy::NewTextElement
            }
        }
    };

    Ok(HitEdits {
        edits,
        strategy,
        removed_empty,
        added_text_elem: strategy == Strategy::NewTextElement,
    })
}

/// 按目标节点的内容形态编码替换串（普通文本要转义，CDATA 段内不能转义）。
fn encode(xml: &[u8], node: &scan::TextNode, replacement: &str) -> Result<Vec<u8>> {
    if node.is_cdata(xml) {
        if replacement.contains("]]>") {
            bail!("替换内容含 \"]>\"，无法写入 CDATA 段");
        }
        Ok(replacement.as_bytes().to_vec())
    } else {
        Ok(scan::escape_text(replacement).into_bytes())
    }
}

/// 求"在开始标签末尾插入属性"的位置——即该标签那个 `>` 之前。
fn attr_insert_pos(xml: &[u8], node: &scan::TextNode) -> Result<usize> {
    if node.content_start == 0 || node.content_start > xml.len() {
        bail!("节点字节区间异常，无法插入 xml:space");
    }
    let at = node.content_start - 1;
    if xml[at] != b'>' {
        bail!(
            "节点开始标签结构异常（位置 {at} 上是 {:?}，不是 '>'），拒绝插入属性",
            xml[at] as char
        );
    }
    Ok(at)
}

/// 把一组编辑应用到原始字节流上。
///
/// 编辑区间**不允许重叠**；同一位置上的**同一份**纯插入会被去重，
/// 这样多条命中落在同一个 `<w:t>` 上时不会写出重复的 `xml:space` 属性。
///
/// ## 同一位置出现**不同**的纯插入 → 直接报错，不静默拼接
///
/// 合并的前提是"两笔插入其实是同一件事"（例如同一个 `xml:space` 被两条命中各加了一次）。
/// 若同一个 `at` 上冒出两份**内容不同**的纯插入，说明编辑生成有 bug：
/// 此时按到达顺序拼接，出来的字节看着"能用"，但顺序错了会让产物悄悄错位——
/// 这是最难查的一类缺陷。所以宁可让这份文件报一个明确的错。
pub fn apply(xml: &[u8], edits: &[Edit]) -> Result<Vec<u8>> {
    let mut sorted: Vec<Edit> = edits.to_vec();
    sorted.sort_by_key(|e| (e.at, e.del));

    let mut merged: Vec<Edit> = Vec::with_capacity(sorted.len());
    for e in sorted {
        if let Some(last) = merged.last_mut() {
            if last.at == e.at && last.del == 0 && e.del == 0 {
                if last.ins != e.ins {
                    bail!(
                        "同一位置出现两份不同的纯插入（位置 {}）：`{}` 与 `{}`。\
                         编辑生成有 bug，拒绝静默拼接（拼接顺序错会让产物悄悄错位）",
                        last.at,
                        String::from_utf8_lossy(&last.ins),
                        String::from_utf8_lossy(&e.ins)
                    );
                }
                // 同一份插入重复到达：去重（保留先到的那笔）
                continue;
            }
            if e.at < last.at + last.del {
                bail!(
                    "编辑区间重叠：{}..{} 与 {}..{}（同一处内容被两条规则同时改写）",
                    last.at,
                    last.at + last.del,
                    e.at,
                    e.at + e.del
                );
            }
        }
        merged.push(e);
    }

    let mut out = Vec::with_capacity(xml.len() + 64);
    let mut pos = 0usize;
    for e in &merged {
        if e.at < pos {
            bail!("编辑位置回退：{pos} → {}", e.at);
        }
        if e.at > xml.len() || e.at + e.del > xml.len() {
            bail!("编辑越界：{}..{}（part 共 {} 字节）", e.at, e.at + e.del, xml.len());
        }
        out.extend_from_slice(&xml[pos..e.at]);
        out.extend_from_slice(&e.ins);
        pos = e.at + e.del;
    }
    out.extend_from_slice(&xml[pos..]);
    Ok(out)
}
