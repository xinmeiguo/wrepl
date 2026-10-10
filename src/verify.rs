//! 验证关卡 1 / 2 —— 格式保全的硬证据，**全程只读文件本身，不调用任何外部程序**。
//!
//! ## 关卡 1：part 级字节比对
//!
//! 除被明确改动的 part 外，其余 part 的解压内容必须 **SHA256 完全一致**；
//! 且「其它」类 part（styles / settings / numbering / media …）**一个字节都不许变**。
//!
//! ## 关卡 2：XML 语义骨架比对
//!
//! 把两份 XML 归一成"骨架 token 流"：**丢弃文本承载元素**
//! （`w:t` / `w:delText` / `w:instrText` 整棵子树）后，其余元素名、属性、
//! 层级、以及元素间空白必须**逐一完全相同**。
//!
//! 为什么这样定义：替换的**唯一合法改动面**就是文本承载元素的内容。
//! 骨架不变 ⇒ `<w:pPr>`、`<w:rPr>`、`<w:tbl>`、`<w:sectPr>` 等格式标记分毫未动。
//! 反过来，任何"多删了一个 run""少了一个 rPr"都会被这一步抓住。
//!
//! 另外单独统计三类承载元素的数量：`delText` / `instrText` 的数量**必须相等**
//! （修订内容与域代码绝不允许被改动），`w:t` 的数量变化则如实报出——只有
//! 「命中全是空元素」这一种情况会新增 `<w:t>`。

use crate::docx::package::{self, PartKind};
use crate::docx::scan;
use crate::engine;
use crate::rules::Rule;
use anyhow::{Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zip::ZipArchive;

/// 关卡 1 中一个 part 的比对结果。
#[derive(Debug, Clone)]
pub struct L1Part {
    pub name: String,
    pub kind: PartKind,
    pub same: bool,
    pub size_a: u64,
    pub size_b: u64,
    /// **解压内容**的 SHA256。`None` = 这一侧的 part 本次**没有解压** ——
    /// 两侧的压缩字节逐字节相同已经足以断定内容相同（见 [`compare_level1`]），
    /// 那时再去解压一遍只为填这一栏就没有意义了。
    pub sha_a: Option<String>,
    pub sha_b: Option<String>,
}

/// 关卡 1 报告。
#[derive(Debug, Clone, Default)]
pub struct L1Report {
    pub parts: Vec<L1Part>,
    pub only_a: Vec<String>,
    pub only_b: Vec<String>,
}

impl L1Report {
    /// **禁止**发生的改动：非文本容器 part（styles / settings / media …）内容变了。
    pub fn forbidden(&self) -> Vec<&L1Part> {
        self.parts
            .iter()
            .filter(|p| !p.same && !p.kind.is_text_bearing())
            .collect()
    }

    /// 发生了改动的 part。
    pub fn changed(&self) -> Vec<&L1Part> {
        self.parts.iter().filter(|p| !p.same).collect()
    }

    pub fn pass(&self) -> bool {
        self.only_a.is_empty() && self.only_b.is_empty() && self.forbidden().is_empty()
    }
}

// ─────────────────────────── 验证快照 ───────────────────────────

/// 一个包快照的**压缩字节从哪儿取**。
///
/// 关卡 1 现在靠「压缩字节逐字节相同」来判"这个 part 没被改动"（见 [`compare_level1`]），
/// 所以快照除了指纹，还得记住**去哪儿取那段压缩字节**。两种来源：
///
/// - [`RawSource::File`]：磁盘上的文件。写副本模式、`wrepl verify` 命令都走这条
///   （原件一直躺在盘上，随时能再读）。
/// - [`RawSource::Mem`]：**改写之前**整份读进内存的原件字节。就地替换专用 ——
///   磁盘上那一份在改写那一刻就被覆盖了，只能靠它。
///
/// ★ 少了 `Mem` 这一支，就地替换的"逐字节比对"会**拿文件跟它自己比**：
/// 两侧的区间都从同一个路径现读，读到的都是改写后的内容，关卡 1 必然全过。
/// 那不是"验证通过"，是**根本没有验证** —— 这正是本模块最要防住的那个陷阱。
#[derive(Debug, Clone)]
pub enum RawSource {
    /// 磁盘上的文件（按 `PartSnap::comp_offset` 现读）
    File(PathBuf),
    /// 已经在内存里的整份原件字节
    Mem(Arc<Vec<u8>>),
}

/// 一个包在**某一时刻**的验证证据：关卡 1 与关卡 2 需要的全部信息。
///
/// ## 为什么必须有它
///
/// **就地替换没有留得住的"原件"。** 磁盘上那一份原文件在改写的那一刻就被覆盖了，
/// 事后再想读一遍原件做比对是不可能的——唯一的窗口是"改写之前"。
///
/// 没有快照时，"验证"只能拿 `(src, dst)` 两条路径去读。就地替换下这两条路径
/// **指的是同一个文件**，等于把文件跟它自己比 ⇒ 关卡 1/2 必然全等通过。
/// 那不是"验证通过"，是**根本没有验证**。文件一旦改名（`src` 在改名后失效），
/// 连这个假通过都维持不住，直接报"打不开文件"。
///
/// 所以就地路径的做法是：**写盘前**采一份快照，写完立刻与产物比完、随即丢弃。
/// 同一时刻内存里只有"线程数 × 一份骨架"的量级，不随文件数增长。
#[derive(Debug, Clone)]
pub struct PkgSnap {
    pub parts: Vec<PartSnap>,
    /// 文本 part 名 → 骨架（关卡 2）。
    /// 读不出 UTF-8 的 part 不进这张表，比对时按"有一侧缺失"跳过——与旧行为一致。
    pub skel: BTreeMap<String, SkelSnap>,
    /// 这份快照的压缩字节来源（见 [`RawSource`]）
    pub raw: RawSource,
}

/// 快照里一个 part 的记账。
#[derive(Debug, Clone)]
pub struct PartSnap {
    pub name: String,
    pub kind: PartKind,
    /// **解压内容**的 SHA256。只有**文本容器**才有：关卡 2 本来就要解压它们做骨架，
    /// 顺手算一次是白得的。其余 part 一律不解压，见 [`PartSnap::comp_offset`]。
    pub content_sha256: Option<String>,
    /// 解压后字节数（当"两侧大小是否一致"的提示用；不做判定依据）
    pub size: u64,
    /// 压缩字节在文件里的起点与长度 —— 关卡 1 逐字节比对的依据。
    /// `None` = 这个条目的位置取不到（畸形 / 特殊容器），比对时退回解压比内容。
    pub comp_offset: Option<u64>,
    pub comp_size: u64,
    /// 压缩方式名。**两侧不同时不能走"压缩字节相同"这条路** ——
    /// 不同的解压方式读同一段字节会得出不同内容，字节相同并不蕴含内容相同。
    pub method: String,
}

/// 骨架 token 流。**全部 token 顺序拼在一个字节缓冲里**，`starts` 记各 token 的起点。
///
/// ## 为什么不是 `Vec<String>`
///
/// 关卡 2 是整条验证链里最贵的一步，而它贵的**不是解析，是分配**：
/// `word/document.xml` 动辄几万个元素与文本事件，原先每个 token 都要
/// `format!` 出一次 `String` 再推进 `Vec<String>`——几万次堆分配。
/// 改成一个可增长的字节缓冲 + 一张 `u32` 起点表之后，推一个 token 只是
/// 一次 `extend_from_slice`，token 的内容与顺序**一字不变**。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Skeleton {
    buf: Vec<u8>,
    /// 第 i 个 token 是 `buf[starts[i] .. starts[i+1]]`（末项到 `buf.len()`）
    starts: Vec<u32>,
}

impl Skeleton {
    /// token 个数。
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// 第 `i` 个 token 的字节。
    pub fn get(&self, i: usize) -> &[u8] {
        let from = self.starts[i] as usize;
        let to = match self.starts.get(i + 1) {
            Some(n) => *n as usize,
            None => self.buf.len(),
        };
        &self.buf[from..to]
    }

    /// 开始一个新 token——之后 `push_*` 进去的字节都算它的。
    fn start_token(&mut self) {
        self.starts.push(self.buf.len() as u32);
    }

    fn push_bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    fn push_byte(&mut self, b: u8) {
        self.buf.push(b);
    }
}

#[derive(Debug, Clone)]
pub struct SkelSnap {
    pub tokens: Skeleton,
    pub carriers: CarrierCounts,
}

/// 采集一个包的验证快照（压缩字节从磁盘现读）。
///
/// ★ **一个包只开一次容器。**
///
/// 原实现是 `inspect(path)`（开 1 次、解压全包算 SHA）＋ 对**每个文本 part 各调一次
/// `read_part(path, name)`** —— 而 `read_part` 每次都重开容器、重解一遍中央目录，
/// 一个包被打开 `1 + N` 次（N ＝ 文本 part 数）。
/// `package::read_parts_where` 的文档注释早把规矩写死了：
/// 「**需要读一个以上条目时一律走这里，只开一次**」，这里补上。
///
/// ★ **只解压文本容器。** 其余 part（styles / settings / media …）一个字节都不解压：
/// 关卡 2 只关心文本容器；关卡 1 对它们的判定改走"压缩字节逐字节相同"，
/// 那比"内容 SHA256 相同"更强（连重新压缩过都排除），也不必解压。
/// 对图片这种 part 省下的是整段解压 + 一次 SHA256 —— 本机 `sha2` 是纯软件实现
/// （约 240 MB/s，且这颗 CPU 没有 SHA 指令加速），这是验证链上最大的一块钱。
///
/// 非 UTF-8 的**文本** part 仍然报错而非静默跳过（与旧行为一致）。
pub fn snapshot(path: &Path) -> Result<PkgSnap> {
    let file = File::open(path).with_context(|| format!("打不开文件：{}", path.display()))?;
    let mut ar = ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("不是合法的 zip/docx 容器：{}", path.display()))?;
    let mut parts = Vec::with_capacity(ar.len());
    let mut skel = BTreeMap::new();
    collect_snapshot(&mut ar, &mut parts, &mut skel)?;
    Ok(PkgSnap {
        parts,
        skel,
        raw: RawSource::File(path.to_path_buf()),
    })
}

/// 就地替换专用：`orig` 是**改写之前**整份读进内存的原件字节。
///
/// 为什么必须把它留到最后：就地替换没有"留得住的左边"。磁盘上那一份在改写那一刻
/// 就被覆盖了，事后再去读"原件里某个 part 的压缩字节区间"，读到的是改写后的文件。
/// 留下这一份，逐字节比对才有一侧是真的"改写前"。
///
/// 顺带省掉一次读盘：这份字节本来就是就地改写要用的（见
/// [`package::rewrite_in_place_with`]）。
pub fn snapshot_from_bytes(path: &Path, orig: Arc<Vec<u8>>) -> Result<PkgSnap> {
    let mut ar = ZipArchive::new(std::io::Cursor::new(orig.as_ref().as_slice()))
        .with_context(|| format!("不是合法的 zip/docx 容器：{}", path.display()))?;
    let mut parts = Vec::with_capacity(ar.len());
    let mut skel = BTreeMap::new();
    collect_snapshot(&mut ar, &mut parts, &mut skel)?;
    Ok(PkgSnap {
        parts,
        skel,
        raw: RawSource::Mem(orig),
    })
}

/// 遍历容器，把每个条目的记账收进 `parts`，文本容器另做骨架进 `skel`。
fn collect_snapshot<R: Read + Seek>(
    ar: &mut ZipArchive<R>,
    parts: &mut Vec<PartSnap>,
    skel: &mut BTreeMap<String, SkelSnap>,
) -> Result<()> {
    for i in 0..ar.len() {
        let mut zf = ar
            .by_index(i)
            .with_context(|| format!("读取第 {} 个条目失败", i))?;
        let name = zf.name().to_string();
        let kind = PartKind::from_name(&name);
        let size = zf.size();
        let comp_size = zf.compressed_size();
        let method = format!("{:?}", zf.compression());
        // `by_index` 一定会把局部头读出来，所以这里拿得到数据起点；
        // 拿不到就记 `None`，比对时退回解压比内容（不猜）。
        let comp_offset = zf.data_start();

        let content_sha256 = if kind.is_text_bearing() {
            let mut buf = Vec::with_capacity(size as usize);
            zf.read_to_end(&mut buf)
                .with_context(|| format!("解压 {name} 失败"))?;
            let s = std::str::from_utf8(&buf).with_context(|| format!("{name} 不是 UTF-8"))?;
            let (tokens, carriers) =
                skeleton(s).with_context(|| format!("{name} 骨架解析失败"))?;
            skel.insert(name.clone(), SkelSnap { tokens, carriers });
            Some(hex::encode(Sha256::digest(&buf)))
        } else {
            None
        };

        parts.push(PartSnap {
            name,
            kind,
            content_sha256,
            size,
            comp_offset,
            comp_size,
            method,
        });
    }
    Ok(())
}

/// 执行关卡 1（按路径读两侧）。
pub fn level1(a: &Path, b: &Path) -> Result<L1Report> {
    let sa = snapshot(a).context("读左侧文件失败")?;
    let sb = snapshot(b).context("读右侧文件失败")?;
    compare_level1(&sa, &sb)
}

/// 关卡 1 的**比对本体**：两份快照的 part 级字节比对。
///
/// ## 未改动的 part 靠什么证明
///
/// 判定分两档，**都是"相同即未改动，不同即改动"的硬判据**，没有任何概率成分：
///
/// 1. **文本容器**：两侧的**解压内容 SHA256**（关卡 2 本来就要解压它们做骨架，
///    这两个值白得）。判据与改造前一模一样。
/// 2. **其余 part**（styles / settings / media / rels …）：比**压缩字节逐字节**。
///    同一条 deflate 流必然解出同一段内容，所以"压缩字节完全相同"蕴含"内容完全相同",
///    而且比内容 SHA256 **更强** —— 它连"重新压缩过但内容没变"都一并排除了。
///    关键在于**它不必解压**：图片这类 part 解压后往往比压缩后大一个数量级，
///    再算一次 SHA256 纯属白干。
///
/// 压缩字节对不上（或压缩方式不同 —— 那时字节相同也不代表内容相同）、
/// 或者拿不到数据起点时，**退回解压比内容 SHA256**，也就是改造前的判据，
/// 只是落到了少数派分支上。
///
/// 判定逻辑只有这一份：`wrepl verify`、写副本的 [`verify_one`]、就地替换的
/// [`verify_snap`] 全部收敛到这里，不允许各写一套。
pub fn compare_level1(a: &PkgSnap, b: &PkgSnap) -> Result<L1Report> {
    let mut rep = L1Report::default();
    let map_b: HashMap<&str, &PartSnap> = b.parts.iter().map(|p| (p.name.as_str(), p)).collect();

    let mut pairs: Vec<(&PartSnap, &PartSnap)> = Vec::new();
    for x in &a.parts {
        match map_b.get(x.name.as_str()) {
            Some(y) => pairs.push((x, y)),
            None => rep.only_a.push(x.name.clone()),
        }
    }
    for y in &b.parts {
        if !a.parts.iter().any(|p| p.name == y.name) {
            rep.only_b.push(y.name.clone());
        }
    }

    // 两份来源各开一次：内存来源直接持有字节，文件来源只拿一个句柄。
    // 逐块比，**不把整份文件读进内存**。
    let mut ra = RawReader::open(&a.raw)?;
    let mut rb = RawReader::open(&b.raw)?;
    let mut cmp = RangeCmp::default();

    for (x, y) in pairs {
        let (same, sha_a, sha_b) = match (&x.content_sha256, &y.content_sha256) {
            // ① 两侧都已经解压过（文本容器）——比内容 SHA256，判据与改造前一致
            (Some(ca), Some(cb)) => (ca == cb, Some(ca.clone()), Some(cb.clone())),
            // ② 至少一侧没解压过 —— 先试"压缩字节逐字节相同"
            _ => {
                let byte_same = x.method == y.method
                    && x.comp_size == y.comp_size
                    && match (x.comp_offset, y.comp_offset) {
                        (Some(oa), Some(ob)) => cmp.eq(&mut ra, oa, &mut rb, ob, x.comp_size),
                        // 取不到数据起点就不猜，直接走内容比对
                        _ => false,
                    };
                if byte_same {
                    (true, None, None)
                } else {
                    // ③ 压缩字节对不上：老老实实解压，比内容
                    let ca = match &x.content_sha256 {
                        Some(s) => s.clone(),
                        None => ra.content_sha(&x.name)?,
                    };
                    let cb = match &y.content_sha256 {
                        Some(s) => s.clone(),
                        None => rb.content_sha(&y.name)?,
                    };
                    (ca == cb, Some(ca), Some(cb))
                }
            }
        };

        rep.parts.push(L1Part {
            name: x.name.clone(),
            kind: x.kind,
            same,
            size_a: x.size,
            size_b: y.size,
            sha_a,
            sha_b,
        });
    }
    Ok(rep)
}

/// 按区间取一个快照的**压缩字节**。`Mem` 直接切片，`File` 走一个复用的句柄。
struct RawReader {
    file: Option<File>,
    mem: Option<Arc<Vec<u8>>>,
}

impl RawReader {
    fn open(src: &RawSource) -> Result<Self> {
        match src {
            RawSource::File(p) => Ok(RawReader {
                file: Some(
                    File::open(p).with_context(|| format!("打不开文件：{}", p.display()))?,
                ),
                mem: None,
            }),
            RawSource::Mem(b) => Ok(RawReader {
                file: None,
                mem: Some(Arc::clone(b)),
            }),
        }
    }

    /// 读 `[off, off + buf.len())`。越界 / 读不出来一律返回 `false` —— 调用方会退回内容比对，
    /// **绝不**把"读不到"当成"相同"。
    fn fill(&mut self, off: u64, buf: &mut [u8]) -> bool {
        if let Some(m) = &self.mem {
            let (from, to) = (off as usize, off as usize + buf.len());
            return match m.get(from..to) {
                Some(s) => {
                    buf.copy_from_slice(s);
                    true
                }
                None => false,
            };
        }
        let f = self.file.as_mut().expect("两种来源必居其一");
        f.seek(SeekFrom::Start(off)).is_ok() && f.read_exact(buf).is_ok()
    }

    /// 解压指定 part 并返回**内容 SHA256**。只在"压缩字节对不上"时才走。
    fn content_sha(&mut self, name: &str) -> Result<String> {
        let mut buf = Vec::new();
        match &self.mem {
            Some(m) => {
                let mut ar = ZipArchive::new(std::io::Cursor::new(m.as_ref().as_slice()))
                    .context("不是合法的 zip/docx 容器")?;
                ar.by_name(name)
                    .with_context(|| format!("docx 内不存在 part：{name}"))?
                    .read_to_end(&mut buf)
                    .with_context(|| format!("解压 {name} 失败"))?;
            }
            None => {
                let f = self.file.as_mut().expect("两种来源必居其一");
                f.seek(SeekFrom::Start(0))?;
                let mut ar =
                    ZipArchive::new(f).context("不是合法的 zip/docx 容器")?;
                ar.by_name(name)
                    .with_context(|| format!("docx 内不存在 part：{name}"))?
                    .read_to_end(&mut buf)
                    .with_context(|| format!("解压 {name} 失败"))?;
            }
        }
        Ok(hex::encode(Sha256::digest(&buf)))
    }
}

/// 逐块比较两个区间。缓冲区在多个 part 之间复用，避免逐个 part 各分配一次。
#[derive(Default)]
struct RangeCmp {
    ba: Vec<u8>,
    bb: Vec<u8>,
}

impl RangeCmp {
    fn eq(&mut self, a: &mut RawReader, a_off: u64, b: &mut RawReader, b_off: u64, len: u64) -> bool {
        const CHUNK: usize = 128 * 1024;
        if len == 0 {
            return true;
        }
        if self.ba.len() < CHUNK {
            self.ba.resize(CHUNK, 0);
            self.bb.resize(CHUNK, 0);
        }
        let mut done = 0u64;
        while done < len {
            let n = ((len - done) as usize).min(CHUNK);
            if !a.fill(a_off + done, &mut self.ba[..n]) {
                return false;
            }
            if !b.fill(b_off + done, &mut self.bb[..n]) {
                return false;
            }
            if self.ba[..n] != self.bb[..n] {
                return false;
            }
            done += n as u64;
        }
        true
    }
}

/// 三类文本承载元素的数量（`w:t` / `w:delText` / `w:instrText`）。
pub type CarrierCounts = [usize; 3];

fn carrier_slot(local: &str) -> Option<usize> {
    match local {
        "t" => Some(0),
        "delText" => Some(1),
        "instrText" => Some(2),
        _ => None,
    }
}

/// 关卡 2 中一个 part 的比对结果。
#[derive(Debug, Clone)]
pub struct L2Part {
    pub name: String,
    pub pass: bool,
    pub tokens_a: usize,
    pub tokens_b: usize,
    pub carriers_a: CarrierCounts,
    pub carriers_b: CarrierCounts,
    /// 骨架中消失的 token——**全部**必须是 run 内的空元素，否则判不通过
    pub deleted: Vec<String>,
    /// 无法解释的差异（出现新结构、或删了不该删的）
    pub bad: Vec<String>,
}

/// 关卡 2 报告。
#[derive(Debug, Clone, Default)]
pub struct L2Report {
    pub parts: Vec<L2Part>,
}

impl L2Report {
    pub fn pass(&self) -> bool {
        self.parts.iter().all(|p| p.pass)
    }
}

/// 执行关卡 2：对两侧都存在、且是文本容器的 part 逐一比对骨架（按路径读两侧）。
pub fn level2(a: &Path, b: &Path) -> Result<L2Report> {
    let sa = snapshot(a).context("读左侧文件失败")?;
    let sb = snapshot(b).context("读右侧文件失败")?;
    Ok(compare_level2(&sa, &sb))
}

/// 关卡 2 的**比对本体**：两份快照的骨架逐 token 比对。
pub fn compare_level2(a: &PkgSnap, b: &PkgSnap) -> L2Report {
    let mut rep = L2Report::default();

    for x in &a.parts {
        if !x.kind.is_text_bearing() {
            continue;
        }
        let (Some(sa), Some(sb)) = (a.skel.get(&x.name), b.skel.get(&x.name)) else {
            continue;
        };
        let (ta, ca) = (&sa.tokens, sa.carriers);
        let (tb, cb) = (&sb.tokens, sb.carriers);

        let (deleted, bad) = compare_skeleton(ta, tb);

        // delText / instrText 数量必须相等（修订内容与域代码绝不允许被改动）
        let mut bad = bad;
        if ca[1] != cb[1] {
            bad.push(format!(
                "w:delText 数量变化 {} → {}（修订删除内容不允许被改动）",
                ca[1], cb[1]
            ));
        }
        if ca[2] != cb[2] {
            bad.push(format!(
                "w:instrText 数量变化 {} → {}（域代码不允许被改动）",
                ca[2], cb[2]
            ));
        }
        if cb[0] < ca[0] {
            bad.push(format!(
                "w:t 数量减少 {} → {}（文本承载元素只应新增，不应被删除）",
                ca[0], cb[0]
            ));
        }

        rep.parts.push(L2Part {
            name: x.name.clone(),
            pass: bad.is_empty(),
            tokens_a: ta.len(),
            tokens_b: tb.len(),
            carriers_a: ca,
            carriers_b: cb,
            deleted,
            bad,
        });
    }

    rep
}

/// 判断 `B` 是否由 `A` 仅通过「删除 run 内空元素」得到。
///
/// 返回 `(被删除的 token, 无法解释的差异)`。
///
/// 做法：把 B 当作 A 的子序列去匹配（贪心取最早位置），
/// A 中没被匹配上的就是"消失的 token"。这样一次比较同时抓住三类问题：
/// 出现的**新**结构（B 里有 A 没有的）、**顺序变化**、以及**删了不该删的元素**。
fn compare_skeleton(a: &Skeleton, b: &Skeleton) -> (Vec<String>, Vec<String>) {
    let mut deleted: Vec<String> = Vec::new();
    let mut bad: Vec<String> = Vec::new();
    let mut i = 0usize;

    for j in 0..b.len() {
        let tb = b.get(j);
        while i < a.len() && a.get(i) != tb {
            deleted.push(token_text(a.get(i)));
            i += 1;
        }
        if i >= a.len() {
            if bad.len() < 20 {
                bad.push(format!("出现了原本没有的结构：{}", token_text(tb)));
            }
            continue;
        }
        i += 1;
    }
    while i < a.len() {
        deleted.push(token_text(a.get(i)));
        i += 1;
    }

    for d in &deleted {
        if !is_allowed_empty_elem(d.as_bytes()) && bad.len() < 20 {
            bad.push(format!("删除了不允许删除的元素：{d}"));
        }
    }

    (deleted, bad)
}

/// 骨架 token → 给人看的文本（骨架里的 token 全部来自 UTF-8 输入，正常不会有替换字符）。
fn token_text(t: &[u8]) -> String {
    String::from_utf8_lossy(t).into_owned()
}

/// 从尾部剥掉 ASCII 空白（等价于 `str::trim_end` 在 token 上的行为）。
fn trim_ascii_end(b: &[u8]) -> &[u8] {
    let mut n = b.len();
    while n > 0 && b[n - 1].is_ascii_whitespace() {
        n -= 1;
    }
    &b[..n]
}

/// 从头部剥掉 ASCII 空白。
fn trim_ascii_start(b: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    &b[i..]
}

/// 允许被删除的元素——**只有** run 内的空元素：
/// 命中的文本里含 `-`（`noBreakHyphen`）或制表符、换行时，删掉元素本身是唯一能"去掉它"的办法。
///
/// 其它任何元素（`<w:r>`、`<w:rPr>`、`<w:p>`、`<w:tbl>` …）被删掉都必须判不通过。
fn is_allowed_empty_elem(tok: &[u8]) -> bool {
    let t = trim_ascii_end(tok);
    // 空元素 token 形如 `<w:tab/>`：至少要有一个 `<`、一个名字、`/>` 三部分。
    // 原始实现会对 `"/>"` 这种退化输入越界，这里直接判不是空元素（更稳，且不可能出现在真实文档里）。
    if t.len() < 3 || !t.ends_with(b"/>") {
        return false; // 带子元素的开始标签不是空元素
    }
    let inner = trim_ascii_start(&t[1..t.len() - 2]);
    // 先切掉属性部分，再剥命名空间前缀
    let name_part = inner
        .split(|b| b.is_ascii_whitespace())
        .next()
        .unwrap_or(inner);
    let local = match name_part.iter().rposition(|b| *b == b':') {
        Some(i) => &name_part[i + 1..],
        None => name_part,
    };
    matches!(local, b"noBreakHyphen" | b"tab" | b"br" | b"cr" | b"softHyphen")
}

/// 把一份 XML 归一成骨架 token 流，并统计三类承载元素数量。
fn skeleton(xml: &str) -> Result<(Skeleton, CarrierCounts)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;

    let mut out = Skeleton::default();
    let mut carriers: CarrierCounts = [0, 0, 0];
    // 元素栈（用于判断"承载元素的父节点是不是 run"）。
    // 只需要回答"父元素是不是 `w:r`"，所以一格一个 bool 就够——不必存元素名。
    let mut stack: Vec<bool> = Vec::new();
    // 正在跳过的承载元素：记录进入时的栈深度
    let mut skip_depth: Option<usize> = None;

    loop {
        let ev = reader.read_event().context("XML 解析失败")?;
        match ev {
            Event::Eof => break,
            Event::Start(e) => {
                let local = e.local_name();
                if skip_depth.is_none() {
                    let parent_is_run = stack.last() == Some(&true);
                    match (parent_is_run, carrier_slot(local.as_ref())) {
                        (true, Some(k)) => {
                            carriers[k] += 1;
                            skip_depth = Some(stack.len());
                        }
                        _ => push_start_tag(&mut out, &e, false),
                    }
                }
                stack.push(local.as_ref() == "r");
            }
            Event::Empty(e) => {
                if skip_depth.is_none() {
                    let local = e.local_name();
                    let parent_is_run = stack.last() == Some(&true);
                    match (parent_is_run, carrier_slot(local.as_ref())) {
                        (true, Some(k)) => carriers[k] += 1,
                        _ => push_start_tag(&mut out, &e, true),
                    }
                }
            }
            Event::End(e) => {
                let local = e.local_name();
                let closing_skip_root =
                    matches!(skip_depth, Some(d) if d + 1 == stack.len());
                stack.pop();
                if closing_skip_root {
                    skip_depth = None;
                } else if skip_depth.is_none() {
                    // 注意：闭合 token 用的是**局部名**（`</p>`），开始 token 用的是
                    // 限定名（`<w:p>`）——这是既有行为，两侧归一方式相同，不许顺手"修"。
                    out.start_token();
                    out.push_bytes(b"</");
                    out.push_bytes(local.as_ref().as_bytes());
                    out.push_byte(b'>');
                }
            }
            Event::Text(_) | Event::CData(_) => {
                if skip_depth.is_none() {
                    out.start_token();
                    out.push_bytes(b"#text");
                }
            }
            _ => {}
        }
    }

    Ok((out, carriers))
}

/// 把一个开始标签（或空元素标签）写成一个骨架 token：
/// 限定名 + 全部属性（顺序原样保留，值不转义处理）。
fn push_start_tag(out: &mut Skeleton, e: &BytesStart<'_>, empty: bool) {
    out.start_token();
    out.push_byte(b'<');
    out.push_bytes(e.name().as_ref().as_bytes());
    for a in e.attributes().flatten() {
        out.push_byte(b' ');
        out.push_bytes(a.key.as_ref().as_bytes());
        out.push_byte(b'=');
        out.push_bytes(a.value.as_ref().as_bytes());
    }
    if empty {
        out.push_bytes(b"/>");
    } else {
        out.push_byte(b'>');
    }
}

// ═══════════════════════════ 残留自检 ═══════════════════════════
//
// 「关卡 1 / 2」证明的是**格式没被动**，证明不了**该改的都改了**。
// 这一层补上后半句：拿规则里的「查找内容」回头扫**整个包**
// （含 docProps / rels / 所有 xml），看还有没有漏网的。
//
// 为什么必须扫全包而不只看可见文本：编号与客户名会出现在
// `docProps/core.xml` 的标题、`app.xml` 的公司字段里，这些不是
// 「文本容器 part」，只看正文会漏判成"干净"。
//
// 为什么复用 `engine::fold` / `find_all` 而不是 `str::contains`：
// 规则可能开着「不区分大小写」或「不分全半角」，用字面 contains
// 会把本该算残留的写法漏掉，反过来也可能误报。必须与替换时同一套语义。

/// 一条规则在产物里的残留。
#[derive(Debug, Clone)]
pub struct ResidueEntry {
    pub rule_id: u32,
    pub needle: String,
    pub count: usize,
    /// 其中落在**可见文本**（正文/页眉页脚/脚注/批注的 `w:t`）里的处数。
    ///
    /// `count - in_text` 全在 XML 标记与属性里（`w:author`、`docProps`、
    /// `settings.xml` 的 rsid 十六进制……）。**只看 in_text 才代表"真的没改到"**，
    /// 所以这一栏必须报告出来，否则用户会被总数字吓到却找不到病根。
    pub in_text: usize,
    /// 出现位置（part 名，最多记 5 处）
    pub where_: Vec<String>,
}

/// 残留自检报告。
#[derive(Debug, Clone, Default)]
pub struct ResidueReport {
    /// 扫过的 part 数（含非文本容器）
    pub parts_scanned: usize,
    pub entries: Vec<ResidueEntry>,
}

impl ResidueReport {
    /// 干干净净 = 所有规则的「查找内容」在产物里一次都不剩。
    pub fn clean(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn total(&self) -> usize {
        self.entries.iter().map(|e| e.count).sum()
    }
    /// 其中落在**可见文本**里的处数——这才是"真的没改到"。
    pub fn text_total(&self) -> usize {
        self.entries.iter().map(|e| e.in_text).sum()
    }
}

/// 取出一个包里**所有可参与匹配的文本**：文本容器走扫描器（跨 run 拼接后），
/// 其余 `.xml` / `.rels` 直接当纯文本过一遍。
/// 返回 `(part 名, 参与匹配的文本, 是否文本容器)`。
fn matchable_parts(path: &Path) -> Result<Vec<(String, String, bool)>> {
    // 一次开容器，把"要扫的"和"要原样看的"一起读出来。
    //
    // 原实现是三次重复劳动：`text_parts()` 走一遍 `inspect()`（全包解压 + 逐个 SHA），
    // 紧接着 `inspect()` 再来一遍，然后每个 part 又各 `read_part` 一次——
    // 一个文件开 N+2 次容器。这里合成一次。
    let raw = package::read_parts_where(path, |name, kind| {
        kind.is_text_bearing() || name.ends_with(".xml") || name.ends_with(".rels")
    })?;

    let mut out = Vec::new();
    for (name, kind, bytes) in raw {
        let Ok(s) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if kind.is_text_bearing() {
            // 文本容器：只取可见文本（`w:t` 等真实渲染出来的字），
            // 这样"查找内容"在 XML 标记里出现不算残留。
            let (paras, _) = scan::scan_part(&name, s)?;
            // ★ 段落之间用 U+2029（PARAGRAPH SEPARATOR）拼接，**不能**用 '\n'：
            // 段内的 `<w:br/>` 在可见文本里映射成 '\n'（与替换引擎一致，
            // 查找串含 \n 时可以命中段内换行）。若段落分隔符也是 '\n'，
            // 含换行的查找串会把"段 N 末尾 + 段 N+1 开头"拼成一次假命中——
            // 残留自检误报。而替换引擎里跨段必不命中，两边语义必须对齐。
            // U+2029 不会出现在折叠后的查找串里（规则文本正常无人输入它），
            // 用它做分隔符即可保证"跨段的拼接命中"在数学上不可能。
            let joined = paras
                .iter()
                .map(|p| p.visible.as_str())
                .collect::<Vec<_>>()
                .join("\u{2029}");
            out.push((name, joined, true));
        } else {
            // 参数/关系/样式：整份文本都算数，逐字找
            out.push((name, s.to_string(), false));
        }
    }
    Ok(out)
}

/// 一条规则在一段文本里的出现次数。
fn count_one(hay: &[char], folded_needle: &[char], whole_word: bool) -> usize {
    if folded_needle.is_empty() {
        return 0;
    }
    engine::find_all(hay, folded_needle, whole_word).len()
}

/// 在整包里找这些规则的「查找内容」还剩多少。
pub fn residue(path: &Path, rules: &[Rule]) -> Result<ResidueReport> {
    let parts = matchable_parts(path)?;
    let mut rep = ResidueReport {
        parts_scanned: parts.len(),
        entries: Vec::new(),
    };

    // 先按「折叠设置」给启用中的规则分组：同一组里的规则共用一份折叠后的文本。
    // 顺序仍然按规则表原序汇总，报告里条目的排列不受影响。
    //
    // **文件名专属规则跳过**：它们一个内容维度都没开，字面文本只可能出现在文件名里，
    // 拿它们去搜 part 内容只会每个文件报一条假残留。判据收在 `Scope::content_empty`。
    let active: Vec<(usize, &Rule)> = rules
        .iter()
        .enumerate()
        .filter(|(_, r)| r.enabled && !r.scope.content_empty())
        .collect();

    // 每条规则每段的计数汇总
    let mut counts: Vec<usize> = vec![0; active.len()];
    let mut in_text: Vec<usize> = vec![0; active.len()];
    let mut where_: Vec<Vec<String>> = vec![Vec::new(); active.len()];

    let mut groups: Vec<((bool, bool), Vec<usize>)> = Vec::new();
    for (ai, (_, r)) in active.iter().enumerate() {
        let key = (r.case_sensitive, r.kana_sensitive);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(ai),
            None => groups.push((key, vec![ai])),
        }
    }

    for (key, idxs) in &groups {
        let (case_sensitive, kana_sensitive) = *key;
        let needles: Vec<Vec<char>> = idxs
            .iter()
            .map(|&ai| engine::fold(&active[ai].1.find, case_sensitive, kana_sensitive))
            .collect();
        for (name, text, is_text) in &parts {
            let hay = engine::fold(text, case_sensitive, kana_sensitive);
            for (n, &ai) in idxs.iter().enumerate() {
                let c = count_one(&hay, &needles[n], active[ai].1.whole_word);
                if c > 0 {
                    counts[ai] += c;
                    if *is_text {
                        in_text[ai] += c;
                    }
                    if where_[ai].len() < 5 {
                        where_[ai].push(format!("{name} ×{c}"));
                    }
                }
            }
        }
    }

    for (ai, (_, r)) in active.iter().enumerate() {
        if counts[ai] > 0 {
            rep.entries.push(ResidueEntry {
                rule_id: r.id,
                needle: r.find.clone(),
                count: counts[ai],
                in_text: in_text[ai],
                where_: std::mem::take(&mut where_[ai]),
            });
        }
    }
    // **按处数从多到少排**。报告里的「备注」只列前几条，若按规则号排，
    // 排在前面的永远是小数目条目，真正的元凶（往往是 `settings.xml` 里
    // 成千上万个 rsid 撞上裸数字规则）反而被截掉，用户会读错病因。
    rep.entries.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| b.in_text.cmp(&a.in_text))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });
    Ok(rep)
}

/// 统计每条规则的「查找内容」(`use_replace = false`) 或「替换为」
/// (`use_replace = true`) 在整包里的出现次数。
///
/// 用途：对比处理前后——`替换为` 的增量应当等于实际替换数。
pub fn count_in_package(path: &Path, rules: &[Rule], use_replace: bool) -> Result<Vec<(u32, usize)>> {
    let parts = matchable_parts(path)?;
    // 与 `residue` 同一套做法：按折叠设置分组，同一组里的规则共用一份折叠后的文本。
    let probes: Vec<(&Rule, Rule)> = rules
        .iter()
        .filter(|r| r.enabled)
        .map(|r| {
            let p = if use_replace {
                Rule {
                    find: r.replace.clone(),
                    replace: String::new(),
                    ..r.clone()
                }
            } else {
                r.clone()
            };
            (r, p)
        })
        .collect();

    let mut totals: Vec<usize> = vec![0; probes.len()];
    let mut groups: Vec<((bool, bool), Vec<usize>)> = Vec::new();
    for (i, (_, p)) in probes.iter().enumerate() {
        if p.find.is_empty() {
            continue;
        }
        let key = (p.case_sensitive, p.kana_sensitive);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(i),
            None => groups.push((key, vec![i])),
        }
    }

    for (key, idxs) in &groups {
        let (case_sensitive, kana_sensitive) = *key;
        let needles: Vec<Vec<char>> = idxs
            .iter()
            .map(|&i| engine::fold(&probes[i].1.find, case_sensitive, kana_sensitive))
            .collect();
        for (_, text, _) in &parts {
            let hay = engine::fold(text, case_sensitive, kana_sensitive);
            for (n, &i) in idxs.iter().enumerate() {
                totals[i] += count_one(&hay, &needles[n], probes[i].1.whole_word);
            }
        }
    }

    Ok(probes
        .iter()
        .enumerate()
        .map(|(i, (r, _))| (r.id, totals[i]))
        .collect())
}

// ═══════════════════════════ 批量验证 ═══════════════════════════

/// 一组（处理前, 处理后）的验证结论。
#[derive(Debug, Clone)]
pub struct PairVerdict {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub l1_pass: bool,
    pub l2_pass: bool,
    pub changed_parts: Vec<String>,
    pub note: String,
}

impl PairVerdict {
    pub fn ok(&self) -> bool {
        self.l1_pass && self.l2_pass
    }
}

/// 验一对（原件, 产物），返回裁决。
///
/// `verify_pairs` 和 `pipeline` 的**写副本**路径共用这一个实现——
/// 那条路径上原件仍在磁盘上，按路径读两侧即可。
///
/// **就地替换不走这里**：它的原件在改写那一刻就没了，必须用
/// [`verify_snap`]（左侧是写盘前扣下的快照）。两条路最终都进 [`judge`]，
/// 结论的措辞与判据只有那一份。
pub fn verify_one(a: &Path, b: &Path) -> PairVerdict {
    let sa = match snapshot(a).context("读左侧文件失败") {
        Ok(v) => v,
        Err(e) => {
            return failed(a, b, format!("读取左侧文件失败：{e:#}"));
        }
    };
    let sb = match snapshot(b).context("读右侧文件失败") {
        Ok(v) => v,
        Err(e) => {
            return failed(a, b, format!("读取右侧文件失败：{e:#}"));
        }
    };
    let l1 = match compare_level1(&sa, &sb) {
        Ok(v) => v,
        Err(e) => {
            return failed(a, b, format!("关卡1 比对失败：{e:#}"));
        }
    };
    judge(&l1, &compare_level2(&sa, &sb), a, b)
}

/// 用**写盘前扣下的左侧快照**验一个产物。
///
/// 就地替换专用：`before` 是原文件被覆盖之前的快照（字节留在内存里，
/// 见 [`snapshot_from_bytes`]），`after` 是覆盖之后的同一路径。
pub fn verify_snap(before: &PkgSnap, after: &Path) -> PairVerdict {
    let sb = match snapshot(after).context("读右侧文件失败") {
        Ok(v) => v,
        Err(e) => {
            return failed(after, after, format!("读取右侧文件失败：{e:#}"));
        }
    };
    let l1 = match compare_level1(before, &sb) {
        Ok(v) => v,
        Err(e) => {
            return failed(after, after, format!("关卡1 比对失败：{e:#}"));
        }
    };
    judge(&l1, &compare_level2(before, &sb), after, after)
}

fn failed(src: &Path, dst: &Path, note: String) -> PairVerdict {
    PairVerdict {
        src: src.to_path_buf(),
        dst: dst.to_path_buf(),
        l1_pass: false,
        l2_pass: false,
        changed_parts: Vec::new(),
        note,
    }
}

/// 由两份报告得出裁决——**结论措辞只在这里产生**。
fn judge(x: &L1Report, y: &L2Report, a: &Path, b: &Path) -> PairVerdict {
    let (l1_pass, l2_pass, changed, note) = (
        x.pass(),
        y.pass(),
        x.changed().iter().map(|p| p.name.clone()).collect(),
        if x.pass() && y.pass() {
            String::new()
        } else {
            let mut m = Vec::new();
            if !x.pass() {
                let f: Vec<String> = x.forbidden().iter().map(|p| p.name.clone()).collect();
                if !f.is_empty() {
                    m.push(format!("关卡1：不应改动的 part 被改 {}", f.join(", ")));
                }
                if !x.only_a.is_empty() {
                    m.push(format!("关卡1：仅原文件有 {}", x.only_a.join(", ")));
                }
                if !x.only_b.is_empty() {
                    m.push(format!("关卡1：仅产物有 {}", x.only_b.join(", ")));
                }
            }
            if !y.pass() {
                let bad: Vec<String> = y
                    .parts
                    .iter()
                    .filter(|p| !p.pass)
                    .flat_map(|p| p.bad.iter().cloned())
                    .take(3)
                    .collect();
                m.push(format!("关卡2：{}", bad.join("；")));
            }
            m.join("　")
        },
    );

    PairVerdict {
        src: a.to_path_buf(),
        dst: b.to_path_buf(),
        l1_pass,
        l2_pass,
        changed_parts: changed,
        note,
    }
}

/// 一次验一整批，替代"外部循环调 verify"。
///
/// 串行版本：给命令行 `verify --batch` 用（那里还要做结构指纹配对，
/// 顺序本身有语义）。`pipeline` 的批量验证走并行版，两者底层是同一次
/// [`verify_one`]，结论必须一致。
pub fn verify_pairs(pairs: &[(PathBuf, PathBuf)]) -> Vec<PairVerdict> {
    pairs.iter().map(|(a, b)| verify_one(a, b)).collect()
}

// ─────────────────────────── 改名后的批量配对 ───────────────────────────

/// 一份 docx 的「结构指纹」——**与文字内容无关**。
///
/// 组成：part 名单 + 每个 part 的指纹。非文本 part 用原始字节 SHA；
/// 文本 part 用**骨架**哈希（`skeleton()` 已把文本承载元素归一成占位符，
/// 属性/层级/元素间空白全部保留）。
///
/// 因此：同一模板只改文字、甚至把文件改了名，指纹仍然**完全不变**。
/// 这正是「产物已改名」时仍能把原件与产物正确配对的依据。
pub fn structure_signature(path: &Path) -> Result<String> {
    let pa = package::inspect(path).with_context(|| format!("读 {} 失败", path.display()))?;
    let mut items: Vec<(String, String)> = Vec::with_capacity(pa.len());

    for x in &pa {
        let fp = if x.kind.is_text_bearing() {
            match package::read_part(path, &x.name).and_then(|raw| {
                std::str::from_utf8(&raw)
                    .map(|s| s.to_string())
                    .map_err(|e| anyhow::anyhow!("{e}"))
            }) {
                Ok(s) => match skeleton(&s) {
                    Ok((toks, carriers)) => {
                        let mut h = Sha256::new();
                        for i in 0..toks.len() {
                            h.update(toks.get(i));
                            h.update(b"\n");
                        }
                        h.update(
                            format!("|c/{}/{}/{}", carriers[0], carriers[1], carriers[2])
                                .as_bytes(),
                        );
                        hex::encode(h.finalize())
                    }
                    // 骨架解析失败：退化为字节指纹（宁可配不上，也不误配）
                    Err(_) => format!("!skel!{}", x.sha256),
                },
                Err(_) => format!("!utf8!{}", x.sha256),
            }
        } else {
            x.sha256.clone()
        };
        items.push((x.name.clone(), fp));
    }

    items.sort();
    let mut h = Sha256::new();
    for (n, fp) in &items {
        h.update(n.as_bytes());
        h.update(b"=");
        h.update(fp.as_bytes());
        h.update(b"\n");
    }
    Ok(hex::encode(h.finalize()))
}

/// 批量配对的完整结论。
#[derive(Debug, Default)]
pub struct BatchPairing {
    /// 最终配对：(原件, 产物)
    pub pairs: Vec<(PathBuf, PathBuf)>,
    /// 其中靠「同名」配上的条数
    pub by_name: usize,
    /// 其中靠「结构指纹」配上的条数（产物已改名）
    pub by_signature: usize,
    /// 指纹歧义，未配对的说明（如"2 个原件指纹相同"）
    pub ambiguous: Vec<String>,
    /// 没能配上的产物
    pub unmatched_products: Vec<PathBuf>,
    /// 没能配上的原件
    pub unmatched_originals: Vec<PathBuf>,
}

fn docx_files_under(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for e in walkdir::WalkDir::new(dir).max_depth(8).follow_links(false) {
        let e = e.with_context(|| format!("遍历目录失败：{}", dir.display()))?;
        if !e.file_type().is_file() {
            continue;
        }
        let p = e.path();
        if !p
            .extension()
            .map(|s| s.eq_ignore_ascii_case("docx"))
            .unwrap_or(false)
        {
            continue;
        }
        let rel = p
            .strip_prefix(dir)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        out.push((rel, p.to_path_buf()));
    }
    out.sort();
    Ok(out)
}

/// 把「原件目录」与「产物目录」配对。
///
/// 两级策略，都不做含糊的猜测：
/// 1. **同名**（相对路径相同）优先——最可靠；
/// 2. 剩下的按 **结构指纹** 配：同一模板只改文字时骨架不变，所以改了名也能配上；
///    但只在「该指纹在原件里唯一、在产物里也唯一」时才配对，否则记为歧义不配。
pub fn pair_dirs(dir_a: &Path, dir_b: &Path) -> Result<BatchPairing> {
    let a_files = docx_files_under(dir_a)?;
    let b_files = docx_files_under(dir_b)?;

    let mut a_by_rel: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (rel, p) in &a_files {
        a_by_rel.insert(rel.clone(), p.clone());
    }
    let mut b_by_rel: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (rel, p) in &b_files {
        b_by_rel.insert(rel.clone(), p.clone());
    }

    let mut rep = BatchPairing::default();

    // ① 同名优先
    for (rel, ap) in &a_by_rel {
        if let Some(bp) = b_by_rel.get(rel) {
            rep.pairs.push((ap.clone(), bp.clone()));
            rep.by_name += 1;
        }
    }

    // ② 剩下的按结构指纹
    let mut rest_a: Vec<(String, PathBuf)> = Vec::new();
    for (rel, p) in &a_by_rel {
        if !b_by_rel.contains_key(rel) {
            rest_a.push((rel.clone(), p.clone()));
        }
    }
    let mut rest_b: Vec<(String, PathBuf)> = Vec::new();
    for (rel, p) in &b_by_rel {
        if !a_by_rel.contains_key(rel) {
            rest_b.push((rel.clone(), p.clone()));
        }
    }

    // 指纹 → 名单（指纹算不出来的记为 None，绝不参与配对）
    let mut sig_a: BTreeMap<String, Vec<(String, PathBuf)>> = BTreeMap::new();
    for (rel, p) in &rest_a {
        if let Ok(s) = structure_signature(p) {
            sig_a.entry(s).or_default().push((rel.clone(), p.clone()));
        } else {
            rep.ambiguous
                .push(format!("原件指纹读取失败（不参与配对）：{rel}"));
        }
    }
    let mut sig_b: BTreeMap<String, Vec<(String, PathBuf)>> = BTreeMap::new();
    for (rel, p) in &rest_b {
        if let Ok(s) = structure_signature(p) {
            sig_b.entry(s).or_default().push((rel.clone(), p.clone()));
        } else {
            rep.ambiguous
                .push(format!("产物指纹读取失败（不参与配对）：{rel}"));
        }
    }

    let mut matched_a: HashSet<String> = HashSet::new();
    let mut matched_b: HashSet<String> = HashSet::new();

    for (sig, av) in &sig_a {
        let Some(bv) = sig_b.get(sig) else { continue };
        if av.len() == 1 && bv.len() == 1 {
            rep.pairs.push((av[0].1.clone(), bv[0].1.clone()));
            rep.by_signature += 1;
            matched_a.insert(av[0].0.clone());
            matched_b.insert(bv[0].0.clone());
        } else {
            rep.ambiguous.push(format!(
                "结构指纹相同、无法唯一配对：原件 {} 个（{}）↔ 产物 {} 个（{}）",
                av.len(),
                av.iter()
                    .map(|(r, _)| r.as_str())
                    .collect::<Vec<_>>()
                    .join("、"),
                bv.len(),
                bv.iter()
                    .map(|(r, _)| r.as_str())
                    .collect::<Vec<_>>()
                    .join("、"),
            ));
        }
    }

    for (rel, p) in &rest_a {
        if !matched_a.contains(rel) {
            rep.unmatched_originals.push(p.clone());
        }
    }
    for (rel, p) in &rest_b {
        if !matched_b.contains(rel) {
            rep.unmatched_products.push(p.clone());
        }
    }

    rep.pairs.sort();
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;

    #[test]
    fn carrier_slots_are_distinct() {
        assert_eq!(carrier_slot("t"), Some(0));
        assert_eq!(carrier_slot("delText"), Some(1));
        assert_eq!(carrier_slot("instrText"), Some(2));
        assert_eq!(carrier_slot("rPr"), None);
    }

    #[test]
    fn skeleton_drops_text_keeps_structure() {
        let a = r#"<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>甲</w:t></w:r></w:p>"#;
        let b = r#"<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>乙丙丁</w:t></w:r></w:p>"#;
        let (ta, _) = skeleton(a).unwrap();
        let (tb, _) = skeleton(b).unwrap();
        assert_eq!(ta, tb, "只改文字时骨架必须相同");

        let c = r#"<w:p><w:r><w:rPr><w:i/></w:rPr><w:t>乙</w:t></w:r></w:p>"#;
        let (tc, _) = skeleton(c).unwrap();
        assert_ne!(ta, tc, "格式标记变了骨架必须不同");
    }

    #[test]
    fn skeleton_counts_carriers() {
        let a = r#"<w:p><w:r><w:t>x</w:t></w:r><w:r><w:delText>y</w:delText></w:r></w:p>"#;
        let (_, c) = skeleton(a).unwrap();
        assert_eq!(c[0], 1);
        assert_eq!(c[1], 1);
    }

    // ═════════════════ 关卡 1：压缩字节比对 ═════════════════
    //
    // 这一组测试钉住的是"就地替换的验证不许拿文件跟自己比"这条底线。
    // 关卡 1 现在靠"压缩字节逐字节相同"来判"没被改动"，
    // 而**改动前的那段压缩字节只存在于覆盖之前** —— 快照里若不把它留住
    // （`RawSource::Mem`），两侧都从同一个路径现读，读到的都是改写后的内容，
    // 关卡 1 必然全过。那不是"验证通过"，是根本没有验证。

    /// 拼一个最小 zip（关卡 1 只认容器结构与 part 名，不要求它真是 Word 文档）。
    fn build_zip(entries: &[(&str, &[u8], CompressionMethod)]) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, data, method) in entries {
            zw.start_file(*name, SimpleFileOptions::default().compression_method(*method))
                .unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    fn tmp_path(tag: &str) -> PathBuf {
        // ★ 不能用 `std::env::temp_dir()`：本机 `%TEMP%` 在 C 盘，而 C 盘禁止
        //   非系统进程写入 —— 沙箱里有时被放行、脱离沙箱跑回归时又被拒，
        //   测试会随"怎么跑的"而红绿不定。测试二进制自己就住在
        //   `target/<profile>/deps/` 下，那个目录**一定可写**（cargo 刚把 exe 写进去）。
        let exe = std::env::current_exe().expect("取当前测试可执行文件路径");
        let dir = exe
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(exe.as_path())
            .join("wrepl-verify-tmp");
        std::fs::create_dir_all(&dir).expect("建测试临时目录");
        dir.join(format!("{tag}.zip"))
    }

    fn doc_xml() -> &'static [u8] {
        br#"<w:document xmlns:w="x"><w:body><w:p><w:r><w:t>abc</w:t></w:r></w:p></w:body></w:document>"#
    }

    /// ★ 底线：左侧快照取自"改写前"的内存字节，磁盘上已经是改写后的内容 ——
    /// 媒体 part 被换掉了，关卡 1 **必须**报出来。
    #[test]
    fn in_memory_before_snapshot_still_detects_a_changed_media_part() {
        let before_bytes = build_zip(&[
            ("word/document.xml", doc_xml(), CompressionMethod::Deflated),
            ("word/media/image1.bin", &[7u8; 4096], CompressionMethod::Deflated),
        ]);
        let after_bytes = build_zip(&[
            ("word/document.xml", doc_xml(), CompressionMethod::Deflated),
            ("word/media/image1.bin", &[9u8; 4096], CompressionMethod::Deflated),
        ]);

        let path = tmp_path("changed-media");
        std::fs::write(&path, &after_bytes).unwrap();

        let before = snapshot_from_bytes(&path, Arc::new(before_bytes)).unwrap();
        let after = snapshot(&path).unwrap();
        let rep = compare_level1(&before, &after).unwrap();

        let media = rep
            .parts
            .iter()
            .find(|p| p.name == "word/media/image1.bin")
            .expect("媒体 part 应当在报告里");
        assert!(
            !media.same,
            "媒体被改了却没报出来 —— 说明关卡 1 拿文件跟自己比了"
        );
        assert!(!rep.pass(), "非文本容器被改动 = 关卡 1 不通过");
        let _ = std::fs::remove_file(&path);
    }

    /// 压缩字节相同 ⇒ 直接判"未改动"，**不再解压**（`sha_*` 留空即证）。
    #[test]
    fn unchanged_media_part_is_settled_by_compressed_bytes() {
        let bytes = build_zip(&[
            ("word/document.xml", doc_xml(), CompressionMethod::Deflated),
            ("word/media/image1.bin", &[3u8; 8192], CompressionMethod::Deflated),
        ]);
        let path = tmp_path("same-media");
        std::fs::write(&path, &bytes).unwrap();

        let a = snapshot(&path).unwrap();
        let b = snapshot(&path).unwrap();
        let rep = compare_level1(&a, &b).unwrap();

        let media = rep
            .parts
            .iter()
            .find(|p| p.name == "word/media/image1.bin")
            .unwrap();
        assert!(media.same);
        assert!(
            media.sha_a.is_none(),
            "压缩字节相同就该走快捷路，不该再解压算内容 SHA"
        );
        assert!(rep.pass());
        let _ = std::fs::remove_file(&path);
    }

    /// 内容一样但**重新压缩过**（Deflated ↔ Stored）：压缩字节必然不同，
    /// 必须退回"解压比内容 SHA256"，结论仍然是"未改动" —— 判据不比改造前弱。
    #[test]
    fn recompressed_but_identical_part_still_counts_as_same() {
        let media: Vec<u8> = (0..20000u32).map(|i| (i % 251) as u8).collect();
        let a_bytes = build_zip(&[("word/media/i.bin", &media, CompressionMethod::Deflated)]);
        let b_bytes = build_zip(&[("word/media/i.bin", &media, CompressionMethod::Stored)]);

        let pa = tmp_path("recomp-a");
        let pb = tmp_path("recomp-b");
        std::fs::write(&pa, &a_bytes).unwrap();
        std::fs::write(&pb, &b_bytes).unwrap();

        let rep = compare_level1(&snapshot(&pa).unwrap(), &snapshot(&pb).unwrap()).unwrap();
        let p = &rep.parts[0];
        assert_eq!(p.name, "word/media/i.bin");
        assert!(p.same, "内容相同就该判相同，压缩方式变了不算改动");
        assert!(
            p.sha_a.is_some() && p.sha_b.is_some(),
            "退回内容比对时应当把两侧的内容 SHA 填上"
        );
        assert!(rep.pass());
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
    }

    /// Stored ↔ Stored 且内容相同 ⇒ 走快捷路；内容不同 ⇒ 报出来。
    #[test]
    fn stored_parts_go_through_the_byte_fast_path() {
        let same = build_zip(&[("word/media/i.bin", &[1u8; 512], CompressionMethod::Stored)]);
        let other = build_zip(&[("word/media/i.bin", &[2u8; 512], CompressionMethod::Stored)]);

        let pa = tmp_path("stored-a");
        let pb = tmp_path("stored-b");
        std::fs::write(&pa, &same).unwrap();
        std::fs::write(&pb, &other).unwrap();

        let rep = compare_level1(&snapshot(&pa).unwrap(), &snapshot(&pb).unwrap()).unwrap();
        assert!(!rep.parts[0].same, "字节不同必须报改动");

        let rep2 = compare_level1(&snapshot(&pa).unwrap(), &snapshot(&pa).unwrap()).unwrap();
        assert!(rep2.parts[0].same);
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
    }

    /// 文本容器**本来就要解压**（关卡 2 要骨架），所以它的判定沿用内容 SHA256 ——
    /// 一改文字就报改动，且两侧的 SHA 都是现成的。
    #[test]
    fn text_parts_are_settled_by_content_sha() {
        let a = build_zip(&[(
            "word/document.xml",
            doc_xml(),
            CompressionMethod::Deflated,
        )]);
        let b = build_zip(&[(
            "word/document.xml",
            br#"<w:document xmlns:w="x"><w:body><w:p><w:r><w:t>XYZ</w:t></w:r></w:p></w:body></w:document>"#,
            CompressionMethod::Deflated,
        )]);

        let pa = tmp_path("text-a");
        let pb = tmp_path("text-b");
        std::fs::write(&pa, &a).unwrap();
        std::fs::write(&pb, &b).unwrap();

        let rep = compare_level1(&snapshot(&pa).unwrap(), &snapshot(&pb).unwrap()).unwrap();
        let p = &rep.parts[0];
        assert!(p.sha_a.is_some() && p.sha_b.is_some());
        assert!(!p.same, "文字改了：关卡 1 报「该 part 有差异」，合法性由关卡 2 判");
        assert!(rep.pass(), "文本容器的改动不算「禁止改动」，合法性由关卡 2 判");
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
    }
}
