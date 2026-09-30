//! docx 包（zip 容器）的读写。
//!
//! 核心原则：**只改该改的字节**。
//! 未被改动的 part 一律通过 `raw_copy_file` 原样搬运，连重新压缩都不做——
//! 这样压缩方式、压缩字节、条目顺序全部保持原状，Word 打开时感知不到任何差异。

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use zip::{ZipArchive, ZipWriter};

/// docx 里一个 part 的静态信息。
#[derive(Debug, Clone)]
pub struct PartInfo {
    /// zip 内路径，如 `word/document.xml`
    pub name: String,
    /// 压缩后字节数
    pub compressed_size: u64,
    /// 解压后字节数
    pub uncompressed_size: u64,
    /// 压缩方式名称（Stored / Deflated / ...）
    pub method: String,
    /// zip 内记录的 CRC32
    pub crc32: u32,
    /// 解压内容 SHA256
    pub sha256: String,
    /// 推断出的 docx 语义分类
    pub kind: PartKind,
}

/// part 的语义分类——决定它受哪个作用域开关管辖。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    /// `word/document.xml` — 正文
    Document,
    /// `word/header*.xml` — 页眉
    Header,
    /// `word/footer*.xml` — 页脚
    Footer,
    /// `word/footnotes.xml` — 脚注
    Footnotes,
    /// `word/endnotes.xml` — 尾注
    Endnotes,
    /// `word/comments.xml` — 批注
    Comments,
    /// 其它（样式、关系、媒体、设置……）——一律不动
    Other,
}

impl PartKind {
    /// 从 zip 内路径推断语义分类。
    pub fn from_name(name: &str) -> Self {
        // 归一化：zip 条目用正斜杠，但防御性处理反斜杠
        let n = name.replace('\\', "/");
        let lower = n.to_ascii_lowercase();

        // 只认 word/ 下的直接子文件，避免把 word/media/document.xml 之类误判
        let stem = lower
            .strip_prefix("word/")
            .unwrap_or(lower.as_str());

        if stem == "document.xml" {
            return PartKind::Document;
        }
        if stem == "footnotes.xml" {
            return PartKind::Footnotes;
        }
        if stem == "endnotes.xml" {
            return PartKind::Endnotes;
        }
        if stem == "comments.xml" {
            return PartKind::Comments;
        }
        // header1.xml / header2.xml ...（不含 header/_rels 下的关系文件）
        if let Some(rest) = stem.strip_prefix("header") {
            if rest.ends_with(".xml") && rest[..rest.len() - 4].chars().all(|c| c.is_ascii_digit()) {
                return PartKind::Header;
            }
        }
        if let Some(rest) = stem.strip_prefix("footer") {
            if rest.ends_with(".xml") && rest[..rest.len() - 4].chars().all(|c| c.is_ascii_digit()) {
                return PartKind::Footer;
            }
        }
        PartKind::Other
    }

    /// 属于「文本容器」——即需要扫描 `<w:t>` 的 part。
    pub fn is_text_bearing(self) -> bool {
        !matches!(self, PartKind::Other)
    }

    pub fn label(self) -> &'static str {
        match self {
            PartKind::Document => "正文",
            PartKind::Header => "页眉",
            PartKind::Footer => "页脚",
            PartKind::Footnotes => "脚注",
            PartKind::Endnotes => "尾注",
            PartKind::Comments => "批注",
            PartKind::Other => "其它",
        }
    }
}

/// 打开 docx，枚举全部 part 并计算指纹。
pub fn inspect(path: &Path) -> Result<Vec<PartInfo>> {
    let file =
        File::open(path).with_context(|| format!("打不开文件：{}", path.display()))?;
    let mut ar = ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("不是合法的 zip/docx 容器：{}", path.display()))?;

    let mut out = Vec::with_capacity(ar.len());
    for i in 0..ar.len() {
        let mut zf = ar
            .by_index(i)
            .with_context(|| format!("读取第 {} 个条目失败", i))?;
        let name = zf.name().to_string();
        let compressed_size = zf.compressed_size();
        let uncompressed_size = zf.size();
        let crc32 = zf.crc32();
        let method = format!("{:?}", zf.compression());

        let mut buf = Vec::with_capacity(uncompressed_size as usize);
        zf.read_to_end(&mut buf)
            .with_context(|| format!("解压 {} 失败", name))?;
        let sha256 = hex::encode(Sha256::digest(&buf));

        out.push(PartInfo {
            kind: PartKind::from_name(&name),
            name,
            compressed_size,
            uncompressed_size,
            method,
            crc32,
            sha256,
        });
    }
    Ok(out)
}

/// 按**物理偏移**（局部头在文件中的位置）返回条目索引顺序。
///
/// 为什么不能直接用中央目录顺序：zip 规范里中央目录顺序才是权威，物理顺序可以不同。
/// Word 写出的 docx 里这两者**经常不一致**（实测 2025-005AUTc 的 OQ 模板物理上
/// `docProps/` 在最前，中央目录里却是 `[Content_Types].xml` 最前）。若按中央目录
/// 顺序重写，整份文件的字节布局会被重排，虽然每个 part 内容不变，但文件级 SHA 会变。
pub fn physical_order<R: Read + std::io::Seek>(ar: &mut ZipArchive<R>) -> Result<Vec<usize>> {
    let mut v: Vec<(u64, usize)> = Vec::with_capacity(ar.len());
    for i in 0..ar.len() {
        let zf = ar
            .by_index(i)
            .with_context(|| format!("读取第 {} 个条目失败", i))?;
        v.push((zf.header_start(), i));
    }
    v.sort_by_key(|t| t.0);
    Ok(v.into_iter().map(|t| t.1).collect())
}

/// 零改动透传：读入一个 docx，原样写出。
///
/// 这是 M1 的验收目标——**输出与输入的每个 part 原始字节完全一致**。
/// 一个连"读进来再写出去"都不改字节的实现，后面才可能只改该改的地方。
pub fn passthrough(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        bail!("输入文件不存在：{}", src.display());
    }
    if let (Ok(a), Ok(b)) = (src.canonicalize(), dst.canonicalize()) {
        if a == b {
            bail!("输出路径与输入路径相同，零改动模式下拒绝写入：{}", src.display());
        }
    }

    let file = File::open(src).with_context(|| format!("打不开文件：{}", src.display()))?;
    let mut ar = ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("不是合法的 zip/docx 容器：{}", src.display()))?;

    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建输出目录失败：{}", parent.display()))?;
        }
    }

    let out = File::create(dst).with_context(|| format!("创建输出文件失败：{}", dst.display()))?;
    let mut zw = ZipWriter::new(BufWriter::new(out));

    // 按物理偏移顺序搬（而非中央目录顺序），保持原文件的条目排布
    let order = physical_order(&mut ar)?;
    for i in order {
        let zf = ar
            .by_index(i)
            .with_context(|| format!("读取第 {} 个条目失败", i))?;
        // 关键：raw_copy_file 直接搬运压缩后的原始数据，不重新压缩
        zw.raw_copy_file(zf)
            .context("原始数据整块拷贝失败")?;
    }

    let mut bw = zw.finish().context("收尾 zip 失败")?;
    bw.flush().context("写入磁盘失败")?;
    drop(bw); // 先落盘关文件，才能回头改中央目录

    // 复原 zip 记账字段（外部属性 / version made by）——目标：文件级 SHA 与原件一致
    match restore_external_attrs(src, dst) {
        Ok(n) => eprintln!("  · 已复原 {n} 个条目的 zip 记账字段（文件级 SHA 应与原件一致）"),
        Err(e) => eprintln!("  · 提示：未能复原 zip 记账字段（{e}）——不影响 part 内容"),
    }
    Ok(())
}

/// 读出指定 part 的**解压后原始字节**。
///
/// 返回的是字节而非 `String`：调用方需要拿到能在其后做字节级偏移定位的原始内容，
/// 任何"先转成字符串再规范化"的动作都会破坏偏移对应关系。
pub fn read_part(path: &Path, name: &str) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("打不开文件：{}", path.display()))?;
    let mut ar = ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("不是合法的 zip/docx 容器：{}", path.display()))?;
    let mut zf = ar
        .by_name(name)
        .with_context(|| format!("docx 内不存在 part：{name}"))?;
    let mut buf = Vec::with_capacity(zf.size() as usize);
    zf.read_to_end(&mut buf)
        .with_context(|| format!("解压 {name} 失败"))?;
    Ok(buf)
}

/// 一次打开容器，读出**全部「文本容器」part** 的解压字节。
///
/// ## 为什么必须单独有这个函数
///
/// 原来 `process_one` 拿 part 名单走的是 [`text_parts`]→[`inspect`]，而 `inspect`
/// 会把**整包**每个条目都解压一遍并算 SHA256 —— 包括 `word/media/` 下的图片。
/// 为了拿一份名单去解压整包媒体，纯属白干；紧接着每个 part 又各开关一次 zip、
/// 再解压一遍。一个文件被打开 N+1 次。
///
/// 这里只开一次、只解压该扫的 part（`document.xml` / 页眉页脚 / 脚注尾注 / 批注），
/// 其它条目连碰都不碰。顺序与 [`text_parts`] 相同（zip 中央目录顺序），
/// 这样命中的排列顺序不会因为换了个读法而变化。
pub fn read_text_parts(path: &Path) -> Result<Vec<(String, PartKind, Vec<u8>)>> {
    read_parts_where(path, |_, kind| kind.is_text_bearing())
}

/// 一次打开容器，读出**所有满足 `want` 的条目**的解压字节，返回 `(名, 类别, 字节)`。
///
/// 热路径上（逐文件改写、残留自检）"先问有哪些 part、再一个个开 zip 去读"
/// 是纯粹的重复劳动：每 `read_part` 都要重开一次容器、重解一遍中央目录。
/// 需要读一个以上条目时一律走这里，只开一次。
///
/// 顺序＝zip 中央目录顺序，与 [`inspect`] / [`text_parts`] 一致。
pub fn read_parts_where<F>(path: &Path, want: F) -> Result<Vec<(String, PartKind, Vec<u8>)>>
where
    F: Fn(&str, PartKind) -> bool,
{
    let file = File::open(path).with_context(|| format!("打不开文件：{}", path.display()))?;
    let mut ar = ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("不是合法的 zip/docx 容器：{}", path.display()))?;

    let mut out = Vec::new();
    for i in 0..ar.len() {
        let mut zf = ar
            .by_index(i)
            .with_context(|| format!("读取第 {} 个条目失败", i))?;
        let name = zf.name().to_string();
        let kind = PartKind::from_name(&name);
        if !want(&name, kind) {
            continue;
        }
        let mut buf = Vec::with_capacity(zf.size() as usize);
        zf.read_to_end(&mut buf)
            .with_context(|| format!("解压 {name} 失败"))?;
        out.push((name, kind, buf));
    }
    Ok(out)
}

/// 把一个 zip 里的全部条目搬进 `zw`，其中 `replaced` 里列出的条目换成新内容。
///
/// 两个落盘入口（写副本 / 就地覆写）共用这一段，避免"两边各写一套、慢慢漂移"。
fn write_entries<R, W>(
    ar: &mut ZipArchive<R>,
    zw: &mut ZipWriter<W>,
    replaced: &HashMap<String, Vec<u8>>,
) -> Result<()>
where
    R: Read + Seek,
    W: Write + Seek,
{
    let order = physical_order(ar)?;
    for i in order {
        let zf = ar
            .by_index(i)
            .with_context(|| format!("读取第 {} 个条目失败", i))?;
        let name = zf.name().to_string();

        match replaced.get(&name) {
            None => {
                // 关键：raw_copy_file 直接搬运压缩后的原始数据，不重新压缩
                zw.raw_copy_file(zf).context("原始数据整块拷贝失败")?;
            }
            Some(bytes) => {
                // 只保留 Stored / Deflated：zip 依赖已裁剪到只剩 deflate，
                // 遇到其它压缩方式一律改写成 Deflated（内容不受影响）。
                let method = match zf.compression() {
                    zip::CompressionMethod::Stored => zip::CompressionMethod::Stored,
                    _ => zip::CompressionMethod::Deflated,
                };
                let mtime = zf.last_modified();
                let unix_mode = zf.unix_mode();

                let mut opts = zip::write::SimpleFileOptions::default().compression_method(method);
                if let Some(t) = mtime {
                    opts = opts.last_modified_time(t);
                }
                if let Some(m) = unix_mode {
                    opts = opts.unix_permissions(m);
                }

                zw.start_file(name.clone(), opts)
                    .with_context(|| format!("写入 {name} 失败"))?;
                zw.write_all(bytes)
                    .with_context(|| format!("写入 {name} 内容失败"))?;
            }
        }
    }
    Ok(())
}

/// 把 `replaced` 里列出的 part 换成新内容，其余 part **按原始压缩字节整块搬运**。
///
/// 这是"只改该改的字节"的落盘环节：被改的 part 只有列出来的那几个，
/// 其余连重新压缩都不做，压缩方式与条目顺序原样保留。
///
/// 注意这只适用于 `dst != src` 的写副本模式：读取是惰性的，边读边写同一个文件
/// 会毁掉还没读到的那部分。就地覆写走 [`rewrite_in_place`]。
pub fn write_with_replacements(
    src: &Path,
    dst: &Path,
    replaced: &HashMap<String, Vec<u8>>,
) -> Result<()> {
    if !src.exists() {
        bail!("输入文件不存在：{}", src.display());
    }
    if let (Ok(a), Ok(b)) = (src.canonicalize(), dst.canonicalize()) {
        if a == b {
            bail!("输出路径与输入路径相同，拒绝写入：{}", src.display());
        }
    }

    let file = File::open(src).with_context(|| format!("打不开文件：{}", src.display()))?;
    let mut ar = ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("不是合法的 zip/docx 容器：{}", src.display()))?;

    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建输出目录失败：{}", parent.display()))?;
        }
    }

    let out = File::create(dst).with_context(|| format!("创建输出文件失败：{}", dst.display()))?;
    let mut zw = ZipWriter::new(BufWriter::new(out));
    write_entries(&mut ar, &mut zw, replaced)?;

    let mut bw = zw.finish().context("收尾 zip 失败")?;
    bw.flush().context("写入磁盘失败")?;
    drop(bw);

    // 未被改动的 part 应保持"连元数据都原样"；被改的 part 也沿用原条目的外部属性
    if let Err(e) = restore_external_attrs(src, dst) {
        eprintln!("  · 提示：未能复原 zip 外部属性（{e}）——不影响 part 内容");
    }
    Ok(())
}

/// 在内存里把新包拼出来（原件字节 → 新包字节），并复原容器层记账字段。
///
/// 拆出来是为了就地覆写：那条路不能边读边写同一个文件，必须先把新内容整个拼好。
fn build_zip_bytes(src_bytes: &[u8], replaced: &HashMap<String, Vec<u8>>) -> Result<Vec<u8>> {
    let mut ar = ZipArchive::new(std::io::Cursor::new(src_bytes))
        .context("不是合法的 zip/docx 容器")?;
    let mut zw = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    write_entries(&mut ar, &mut zw, replaced)?;

    let cur = zw.finish().context("收尾 zip 失败")?;
    let mut out = cur.into_inner();

    // 记账字段在内存里直接补，不用把新文件再读一遍
    if let Err(e) = fix_cd(src_bytes, 0, &mut out, 0) {
        eprintln!("  · 提示：未能复原 zip 记账字段（{e}）——不影响 part 内容");
    }
    Ok(out)
}

/// 就地改写 `path` **本身**：不新建临时文件、不 rename，直接在原文件上覆盖。
///
/// ## 为什么不能复用 [`write_with_replacements`]
///
/// zip 读取是**惰性**的：`raw_copy_file` 搬到哪个条目才去读哪一段原始数据。
/// 若 `dst == src`，`File::create` 一截断，后面还没读的字节就没了——
/// 所以必须先在内存里把新包拼完，再一次性覆盖回去。代价是峰值多占一份文件大小的内存
/// （docx 通常几 MB，可接受）。
///
/// ## 换来的东西
///
/// 每处理一个文件少**新建一个文件**（原来要写 `.docx.wrepl-tmp` 再 rename 顶掉）。
/// 在"新建文件有固定开销"的环境里（实时扫描、网络盘、机械盘）这一步省得很实在；
/// 写失败时还能拿内存里的原件字节尽量写回去。备份是否另留一份由调用方决定
/// （`pipeline::Options::backup`，默认关）。
///
/// ## 风险模型的变化（明白之后再用）
///
/// 原来中转 + 同目录 rename 是原子的：要么换成功，要么原件原封不动。
/// 直接覆盖没有这个性质——中途失败会留下半个文件。所以：
/// ① 调用方可以先用 `backup` 留一份 `.bak`（默认不留，留不留是调用方的选择）；
/// ② 这里写失败会先尝试把原件写回去，再报错。
pub fn rewrite_in_place(path: &Path, replaced: &HashMap<String, Vec<u8>>) -> Result<()> {
    let orig = std::fs::read(path).with_context(|| format!("读取失败：{}", path.display()))?;
    let out = build_zip_bytes(&orig, replaced)?;
    if let Err(e) = write_at(path, 0, &out, true) {
        let _ = write_at(path, 0, &orig, true);
        return Err(e).with_context(|| format!("就地改写 {} 失败", path.display()));
    }
    Ok(())
}

/// 列出全部「文本容器」part 的 `(名称, 语义分类)`，按 zip 内顺序。
pub fn text_parts(path: &Path) -> Result<Vec<(String, PartKind)>> {
    Ok(inspect(path)?
        .into_iter()
        .filter(|p| p.kind.is_text_bearing())
        .map(|p| (p.name, p.kind))
        .collect())
}

/// 逐 part 比对两个 docx 的解压内容，返回 `(part 名, 左 SHA, 右 SHA)` 的差异列表。
pub fn diff_parts(a: &Path, b: &Path) -> Result<Vec<(String, String, String)>> {
    let la = inspect(a)?;
    let lb = inspect(b)?;
    let mut diffs = Vec::new();

    let map_b: std::collections::HashMap<&str, &PartInfo> =
        lb.iter().map(|p| (p.name.as_str(), p)).collect();

    for pa in &la {
        match map_b.get(pa.name.as_str()) {
            Some(pb) => {
                if pa.sha256 != pb.sha256 {
                    diffs.push((pa.name.clone(), pa.sha256.clone(), pb.sha256.clone()));
                }
            }
            None => diffs.push((pa.name.clone(), pa.sha256.clone(), "<缺失>".into())),
        }
    }
    for pb in &lb {
        if !la.iter().any(|p| p.name == pb.name) {
            diffs.push((pb.name.clone(), "<缺失>".into(), pb.sha256.clone()));
        }
    }
    Ok(diffs)
}

// ─────────────────────────── zip 记账字段复原 ───────────────────────────
//
// 为什么需要这一段：zip crate 的 `ZipFile::options()` 里有一行
//     .unix_permissions(self.unix_mode().unwrap_or(0o644) | ffi::S_IFREG)
// 它会**无条件**把 Unix 权限位写进中央目录的 external file attributes。
// 而 Word 生成的 docx 通常只带 DOS 归档位（0x20），于是"读进来再写出去"虽然
// 每个 part 的字节都一样，整份文件的 SHA256 却会变。
//
// 这些字段 Windows / Word 根本不读，功能上零影响；但**文件级 SHA 一致**是最硬的
// 格式保全证据，用户拿 sha256sum 一比就能自证。所以这里在写完之后统做三件事：
//   ① 按 part 名回填 external file attributes（CD 偏移 38）
//   ② 按 part 名回填 version made by（CD 偏移 4）
//   ③ 把中央目录记录的排列还原成原件的顺序
//
// 关于 ③：zip 规范里中央目录顺序才是权威，物理顺序允许不同。实测 Word/公司系统
// 写出的 docx 里两者**经常不一致**（2025-005AUTc 的 OQ 模板物理上 `docProps/`
// 在最前，中央目录里却是 `[Content_Types].xml` 最前）。数据段按物理顺序、中央目录
// 按原顺序，二者都还原，整份文件才能逐字节相同。

/// 中央目录里一条记录的完整信息。
struct CdEntry {
    name: String,
    /// 该记录在文件中的起始偏移与总长度
    start: usize,
    len: usize,
    /// version made by（偏移 4，2 字节）——纯声明字段，Word 不读
    made_by: u16,
    made_by_pos: usize,
    /// internal file attributes（偏移 36，2 字节）——bit0 = 文本文件
    internal_attr: u16,
    internal_attr_pos: usize,
    /// 外部属性（偏移 38，4 字节）
    ext_attr: u32,
    /// 外部属性在文件字节中的绝对偏移（4 字节）
    ext_attr_pos: usize,
}

/// 读小端 u16。
fn rd16(buf: &[u8], o: usize) -> usize {
    u16::from_le_bytes([buf[o], buf[o + 1]]) as usize
}

/// 读小端 u32。
fn rd32(buf: &[u8], o: usize) -> usize {
    u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]) as usize
}

/// 定位 EOCD（End Of Central Directory，签名 `PK\x05\x06`）。
/// 允许结尾有注释，故从尾部向前搜。
fn find_eocd(buf: &[u8]) -> Result<usize> {
    const SIG: &[u8; 4] = b"PK\x05\x06";
    let n = buf.len();
    if n < 22 {
        bail!("文件太小，不是合法 zip");
    }
    let from = n.saturating_sub(22 + 0xFFFF);
    let mut i = n - 22;
    loop {
        if &buf[i..i + 4] == SIG {
            return Ok(i);
        }
        if i == from {
            bail!("找不到 zip 中央目录结束记录（EOCD）");
        }
        i -= 1;
    }
}

/// 解析 `buf` 里的中央目录，按顺序返回每条记录，外加**中央目录在 `buf` 内的起始偏移**。
///
/// `base` 是 `buf[0]` 在**文件**中的绝对偏移——EOCD 里记的中央目录偏移是绝对偏移，
/// 而 `buf` 可能只是从某处截的一段。两个量必须分清：
/// **缓冲区基址（`base`）** 和 **目录在缓冲区里的位置（`cd_off - base`）**。
/// 把后者当成前者用，改写的就不是中央目录了（整份文件当缓冲区时二者明显不等）。
/// 返回的 `CdEntry` 里的位置**一律相对 `buf`**，方便直接在 `buf` 上改字节。
fn central_directory_at(buf: &[u8], base: usize) -> Result<(Vec<CdEntry>, usize)> {
    let eocd = find_eocd(buf)?;
    let total = rd16(buf, eocd + 10);
    let cd_off = rd32(buf, eocd + 16);
    if total == 0xFFFF || cd_off == 0xFFFF_FFFF {
        bail!("ZIP64 容器，跳过外部属性复原");
    }
    let start = cd_off
        .checked_sub(base)
        .context("中央目录偏移小于所在区段起点")?;

    let mut out = Vec::with_capacity(total);
    let mut p = start;
    for _ in 0..total {
        if p + 46 > buf.len() || &buf[p..p + 4] != b"PK\x01\x02" {
            bail!("中央目录记录结构异常");
        }
        let name_len = rd16(buf, p + 28);
        let extra_len = rd16(buf, p + 30);
        let cmt_len = rd16(buf, p + 32);
        let name_bytes = buf
            .get(p + 46..p + 46 + name_len)
            .context("中央目录名称越界")?;
        let rec_len = 46 + name_len + extra_len + cmt_len;
        out.push(CdEntry {
            name: String::from_utf8_lossy(name_bytes).into_owned(),
            start: p,
            len: rec_len,
            made_by: u16::from_le_bytes([buf[p + 4], buf[p + 5]]),
            made_by_pos: p + 4,
            internal_attr: u16::from_le_bytes([buf[p + 36], buf[p + 37]]),
            internal_attr_pos: p + 36,
            ext_attr: u32::from_le_bytes([buf[p + 38], buf[p + 39], buf[p + 40], buf[p + 41]]),
            ext_attr_pos: p + 38,
        });
        p += rec_len;
    }
    Ok((out, start))
}

/// 从文件里**只读出中央目录那一段**（含 EOCD 与尾部注释），不把整份文件读进内存。
///
/// 回填记账字段要动的只有中央目录里那几十~几百字节，原先却把原件整份读、产物整份读、
/// 产物整份写——两边各几十 MB 的白活。这里改成两段定长读：
/// 先读文件尾 64 KB 定位 EOCD 拿到中央目录偏移，再从该偏移读到文件末尾。
struct CdRegion {
    /// 该段在文件里的起始偏移
    off: usize,
    /// 从 `off` 到文件末尾的原始字节
    bytes: Vec<u8>,
}

fn read_cd_region(path: &Path) -> Result<CdRegion> {
    let mut f = File::open(path).with_context(|| format!("打不开文件：{}", path.display()))?;
    let len = f
        .metadata()
        .with_context(|| format!("取文件长度失败：{}", path.display()))?
        .len() as usize;
    if len < 22 {
        bail!("文件太小，不是合法 zip：{}", path.display());
    }

    let tail_n = len.min(22 + 0xFFFF);
    let mut tail = vec![0u8; tail_n];
    f.seek(SeekFrom::Start((len - tail_n) as u64))?;
    f.read_exact(&mut tail)?;

    let eocd = find_eocd(&tail)?;
    let total = rd16(&tail, eocd + 10);
    let cd_off = rd32(&tail, eocd + 16);
    if total == 0xFFFF || cd_off == 0xFFFF_FFFF {
        bail!("ZIP64 容器，跳过外部属性复原");
    }
    if cd_off > len {
        bail!("中央目录偏移越界：{cd_off} > {len}");
    }

    let mut bytes = vec![0u8; len - cd_off];
    f.seek(SeekFrom::Start(cd_off as u64))?;
    f.read_exact(&mut bytes)?;
    Ok(CdRegion {
        off: cd_off,
        bytes,
    })
}

/// 把 `cur` 这条记录的三个记账字段按 `orig` 回填进 `buf`；有改动返回 true。
fn patch_fields(buf: &mut [u8], cur: &CdEntry, orig: &CdEntry) -> bool {
    let mut touched = false;
    if orig.ext_attr != cur.ext_attr {
        buf[cur.ext_attr_pos..cur.ext_attr_pos + 4].copy_from_slice(&orig.ext_attr.to_le_bytes());
        touched = true;
    }
    if orig.made_by != cur.made_by {
        buf[cur.made_by_pos..cur.made_by_pos + 2].copy_from_slice(&orig.made_by.to_le_bytes());
        touched = true;
    }
    if orig.internal_attr != cur.internal_attr {
        buf[cur.internal_attr_pos..cur.internal_attr_pos + 2]
            .copy_from_slice(&orig.internal_attr.to_le_bytes());
        touched = true;
    }
    touched
}

/// 在两个 zip 字节流上做记账字段复原 + 中央目录记录顺序还原。
///
/// `src` 是原件、`dst` 是被改写的产物；`*_base` 是各自缓存在文件中的绝对起始偏移
/// （整份文件当缓存时是 0，只截了中央目录区段时是该区段偏移）。
/// 返回被修正的条目数（含顺序调整）。
fn fix_cd(src: &[u8], src_base: usize, dst: &mut [u8], dst_base: usize) -> Result<usize> {
    let (src_entries, _) = central_directory_at(src, src_base)?;
    let (mut dst_entries, dst_cd_start) = central_directory_at(dst, dst_base)?;

    let index: HashMap<&str, &CdEntry> = src_entries
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();

    // ① 字段回填
    let mut fixed = 0usize;
    for e in &dst_entries {
        let Some(orig) = index.get(e.name.as_str()) else {
            continue;
        };
        if patch_fields(dst, e, orig) {
            fixed += 1;
        }
    }

    // ② 还原中央目录记录顺序
    let by_name: HashMap<&str, &CdEntry> = dst_entries
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();
    let dst_cd_len: usize = dst_entries.iter().map(|e| e.len).sum();
    let mut reordered = Vec::with_capacity(dst_cd_len);
    let mut complete = true;
    for e in &src_entries {
        match by_name.get(e.name.as_str()) {
            Some(x) => reordered.extend_from_slice(&dst[x.start..x.start + x.len]),
            None => {
                complete = false;
                break;
            }
        }
    }
    // 只在"两侧条目完全对应、且记录总长不变"时改写，避免任何结构风险
    if complete
        && reordered.len() == dst_cd_len
        && dst_cd_start + dst_cd_len <= dst.len()
    {
        if dst[dst_cd_start..dst_cd_start + dst_cd_len] != reordered[..] {
            dst[dst_cd_start..dst_cd_start + dst_cd_len].copy_from_slice(&reordered);
            fixed += 1;
            // 记录换了位置 → 里面的字段位置全变了，重新定位后再补一次
            let (again, _) = central_directory_at(dst, dst_base)?;
            dst_entries = again;
            for e in &dst_entries {
                let Some(orig) = index.get(e.name.as_str()) else {
                    continue;
                };
                patch_fields(dst, e, orig);
            }
        }
    }
    Ok(fixed)
}

/// 把 `src` 里各 part 的 zip 记账字段，按名称回填到 `dst` 的中央目录，并把中央
/// 目录记录的排列还原成原件的顺序。返回被修正的条目数（含顺序调整）。
/// 任何异常都不影响已写好的 part 内容。
///
/// 只读写两个文件的中央目录区段（见 [`read_cd_region`]），且**只在确实有改动时**落盘。
pub fn restore_external_attrs(src: &Path, dst: &Path) -> Result<usize> {
    let s = read_cd_region(src)?;
    let mut d = read_cd_region(dst)?;
    let fixed = fix_cd(&s.bytes, s.off, &mut d.bytes, d.off)?;
    if fixed > 0 {
        write_at(dst, d.off as u64, &d.bytes, false)?;
    }
    Ok(fixed)
}

/// 把 `data` 写到 `path` 的 `off` 处，`truncate` 为真时顺手把文件截到新长度。
///
/// ## 为什么不用 `std::fs::write`
///
/// 它是 `CREATE_ALWAYS`：先把文件截断到 0 再写。刚落盘的产物在 Windows 上经常
/// 正被杀软实时防护打开扫描，那一瞬间文件是独占的，写请求直接被拒 ——
/// 于是"zip 记账字段没复原"，同一批输入两次跑出来的字节就不一样了。
/// 实测在连续 11 次运行里撞到过 1 次（GUI 与 CLI 都会）。这种"偶发的不一致"
/// 最伤归档：报告说两次结果相同，SHA 却不相同。
///
/// ## 两条改动
///
/// 1. **只写该写的那一段**：记账字段回填时文件长度不变，`OpenOptions::write`
///    打开后 `seek` 过去覆盖即可，不必先清零再重建（少一个"半截文件"的窗口）。
/// 2. **退避重试**：独占通常只持续几十毫秒，重试 6 次（15→480ms）足够跨过去。
fn write_at(path: &Path, off: u64, data: &[u8], truncate: bool) -> Result<()> {
    let mut last: Option<std::io::Error> = None;
    for attempt in 0..6u32 {
        let r = (|| -> std::io::Result<()> {
            let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
            f.seek(std::io::SeekFrom::Start(off))?;
            f.write_all(data)?;
            if truncate {
                f.set_len(off + data.len() as u64)?;
            }
            f.flush()?;
            Ok(())
        })();
        match r {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(15 * (1 << attempt)));
            }
        }
    }
    Err(anyhow::Error::from(last.expect("循环至少跑过一次")))
        .with_context(|| format!("回写 {} 失败（已重试 6 次）", path.display()))
}
