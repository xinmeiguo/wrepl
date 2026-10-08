//! 处理流水线：收集目标 → 逐文件改写 → 文件名同步改名 → 执行后验证 → 汇总。
//!
//! ## 为什么这一层必须在库里
//!
//! 命令行（`src/main.rs`）和图形界面（`src/gui/main.rs`）**共用本模块**。
//! 只要有一个前端自己实现「找文件 / 落盘 / 改名 / 验证」，两边就会慢慢漂移，
//! 最后出现"命令行跑出来是这样、界面跑出来是那样"——这类问题几乎无法排查。
//! 所以：前端只负责**收集参数**和**展示结果**，实质动作全在这里。
//!
//! ## 失败隔离
//!
//! 单个文件出错不中断整批：出错文件记成 [`Outcome::status`] = `ERROR`，
//! 原因写进 `note`，其余文件照跑。这样一份交付包里有个坏文件时，
//! 仍然能拿到其余 25 个的产物和一份完整的报告。

use crate::docx::package;
use crate::engine::{self, Hit};
use crate::naming;
use crate::report::{NameRow, VerifyRow};
use crate::rules::Rule;
use crate::verify;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 被跳过的文件及原因（进报告，方便归档时说明"这个为什么没处理"）。
#[derive(Debug, Clone)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

/// 单个文件的处理结果。
#[derive(Debug, Clone)]
pub struct Outcome {
    pub src: PathBuf,
    pub dst: Option<PathBuf>,
    /// `OK` / `NO_MATCH` / `NO_CHANGE` / `CONFLICT` / `DRY_RUN` / `ERROR`
    pub status: &'static str,
    pub hits: Vec<Hit>,
    pub parts: Vec<String>,
    pub conflicts: usize,
    pub applied: usize,
    pub note: String,
    pub sha_before: String,
    pub sha_after: String,
    pub size_kb: f64,
    pub mtime: String,
    pub rule_count: usize,
    /// 就地替换时**在写盘前采下、写盘后立刻比完**的验证裁决。
    ///
    /// 就地路径的原件在改写那一刻就没了，事后再读 `src` 只会读到产物本身
    /// （或改名后的空路径）。所以这一份必须在 `process_one` 里当场算出来带着走。
    /// 写副本路径不用它（原件还在磁盘上，事后按路径验即可）。
    pub verdict: Option<verify::PairVerdict>,
    /// 本次是「未改动 → 原样复制到输出目录」（勾了**完整镜像**才会有）。
    ///
    /// 它与 [`Outcome::status`] 是**正交**的两件事：`status = NO_MATCH` 说的是
    /// "一条规则都没命中"，`mirrored` 说的是"即便如此，产物路径上也确实有文件了"。
    /// 凡是要判断"产物路径上有没有本次写出来的文件"，都要走
    /// [`Outcome::produced`]——只看 `status == "OK"` 会把镜像产物漏掉。
    pub mirrored: bool,
    /// **本轮是否真的往磁盘上写过**（就地覆盖 / 写副本 / 完整镜像复制都算）。
    ///
    /// 这是 [`Outcome::produced`] 的**唯一**依据。"有没有落盘"必须与
    /// [`Outcome::status`] 分开，因为二者并不等价：一个文件可能**一部分规则落了笔、
    /// 另一部分命中区间重叠**，此时 `status = CONFLICT`（表达"存在冲突"），可文件
    /// 确实被改写了。若拿 `status == "OK"` 当判据，这种真产物会被误判成"未产出"——
    /// 于是不改名、跳过格式验证，报告备注还会写成"输出目录里已有同名旧文件，
    /// 本次未覆盖"，与磁盘实际状态正好相反。
    pub written: bool,
}

impl Outcome {
    /// 某个规则在本文件里的命中条数（界面规则表用）。
    pub fn hits_of_rule(&self, rule_id: u32) -> usize {
        self.hits.iter().filter(|h| h.rule_id == rule_id).count()
    }

    /// 本次运行**真的写出了** `dst`。
    ///
    /// 判据是"本轮真的落过盘"（[`Outcome::written`]），不是"路径上碰巧有文件"——
    /// 输出目录里可能躺着上一轮留下的同名旧文件，那不是本次产物，不该被改名、
    /// 也不该被拿去验证。
    ///
    /// 落地形式有三种，**都算**：按规则改写后落盘（`OK`）、部分规则落笔又部分冲突
    /// （`CONFLICT`——文件确实被改写过，详见 [`Outcome::written`]）、以及完整镜像的
    /// 原样复制（[`Outcome::mirrored`]）。
    pub fn produced(&self) -> bool {
        self.written
    }

    fn errored(src: &Path, rules: &[Rule], msg: String) -> Self {
        let md = std::fs::metadata(src).ok();
        Self {
            src: src.to_path_buf(),
            dst: None,
            status: "ERROR",
            hits: Vec::new(),
            parts: Vec::new(),
            conflicts: 0,
            applied: 0,
            note: msg,
            sha_before: String::new(),
            sha_after: String::new(),
            size_kb: md.as_ref().map(|m| m.len() as f64 / 1024.0).unwrap_or(0.0),
            mtime: md
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| format_epoch(d.as_secs()))
                .unwrap_or_default(),
            rule_count: rules.iter().filter(|r| r.enabled).count(),
            verdict: None,
            mirrored: false,
            written: false,
        }
    }
}

/// 一次运行的参数。前端把用户的选择映射成这个结构，别再自己拼逻辑。
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub recursive: bool,
    pub exclude: Vec<String>,
    pub chain: bool,
    /// 输出副本目录。与 `in_place` 互斥。
    pub out_dir: Option<PathBuf>,
    /// 就地替换源文件（在原文件上直接覆盖改写）。
    pub in_place: bool,
    /// 就地替换时，是否为每个文件保留一份 `.docx.bak` 备份。
    ///
    /// `false`（默认）：源目录不留任何多余文件；写盘失败时靠内存里的原件字节写回。
    /// `true`：只在**首次**生成——重复跑同一批时 `.bak` 始终是最初那一版。
    /// 写副本（`out_dir`）模式与本项无关。
    pub backup: bool,
    pub dry_run: bool,
    pub rename_files: bool,
    pub verify_after: bool,
    /// 文件级并行的线程数。`0` = 按本机可用并行度自动决定；`1` = 强制串行。
    pub threads: usize,
    /// 规则命中区间重叠时的裁决方式。
    ///
    /// - `false`（默认）：**两条都不替换**——"不猜"，把冲突如实报出来让人去改规则表。
    /// - `true`：**最长匹配优先**——只让「查找内容更长」的那条落笔，被挤掉的记进报告。
    ///
    /// 两者都只看规则集合与命中区间，与文件、线程数无关；默认关闭时行为与旧版逐字节一致。
    pub longest_first: bool,
    /// **完整镜像**：写副本模式下，一处都没改动的文件也原样复制到输出目录，
    /// 文件名同样按规则归一。
    ///
    /// - `false`（默认）：输出目录里**只有本次真正改过的文件**——搬过去只会让
    ///   "哪些动过"变模糊，这是既定设计（回归第 10 组钉着）。
    /// - `true`：输出目录是输入目录的完整镜像，可以直接当交付包拿走。
    ///
    /// 只对写副本模式有意义（就地替换本来就在原地，谈不上镜像）。预演不落盘，
    /// 恒不生效。
    pub mirror: bool,
}

/// 一次运行的完整结果。
#[derive(Debug, Default)]
pub struct RunResult {
    pub outcomes: Vec<Outcome>,
    pub skipped: Vec<Skipped>,
    pub name_rows: Vec<NameRow>,
    pub verify_rows: Vec<VerifyRow>,
    /// 本次实际使用的并行线程数（1 = 串行）。前端把它报出来，
    /// 免得"到底有没有并行"只能靠猜。
    pub threads: usize,
}

impl RunResult {
    pub fn total_hits(&self) -> usize {
        self.outcomes.iter().map(|o| o.hits.len()).sum()
    }
    pub fn total_applied(&self) -> usize {
        self.outcomes.iter().map(|o| o.applied).sum()
    }
    pub fn total_conflicts(&self) -> usize {
        self.outcomes.iter().map(|o| o.conflicts).sum()
    }
    pub fn failed(&self) -> usize {
        self.outcomes.iter().filter(|o| o.status == "ERROR").count()
    }
    pub fn ok(&self) -> usize {
        self.outcomes.iter().filter(|o| o.status == "OK").count()
    }
    pub fn verify_bad(&self) -> usize {
        self.verify_rows.iter().filter(|v| !v.ok()).count()
    }
}

/// 跑一整批。
///
/// `on_file` 每处理完一个文件被调用一次，供前端打进度（CLI 打印、GUI 写日志）。
/// 第一个参数是**已完成序数**（1,2,3…），不是文件下标——并行下完成顺序本来
/// 就不等于目录顺序，报下标反而像是乱了。总数是第二个参数。
///
/// ## 执行阶段划分（顺序不能动）
///
/// 1. **收集目标**（串行）：walkdir 扫盘 + 两道闸门。扫盘本身有 IO，但
///    只有一次 metadata 遍历，相对后面每个文件解压+重压缩的量可以忽略。
/// 2. **预分配输出路径**（串行）：见 `map_out` 的注释，这一步**必须**串行。
/// 3. **逐文件改写**（并行）：`parallel_map`，本批最重的一段。
/// 4. **改名**（串行）：文件名是全局命名空间，必须串行消解重名。
/// 5. **验证**（并行）：读写全在包内，各文件之间无依赖。
pub fn run(
    paths: &[PathBuf],
    rules: &[Rule],
    opts: &Options,
    on_file: &(dyn Fn(usize, usize, &Outcome) + Sync),
) -> Result<RunResult> {
    if !opts.in_place && opts.out_dir.is_none() && !opts.dry_run {
        bail!("必须二选一：就地替换（in_place）或输出目录（out_dir）；只有预演可以不选");
    }
    if opts.in_place && opts.out_dir.is_some() {
        bail!("输出目录与就地替换不能同时使用");
    }

    let (files, skipped) = collect_targets(paths, opts)?;
    let threads_used = resolve_threads(opts.threads, files.len());
    let mut res = RunResult {
        outcomes: Vec::with_capacity(files.len()),
        skipped,
        name_rows: Vec::new(),
        verify_rows: Vec::new(),
        threads: threads_used,
    };

    // ── 阶段 2：串行预分配输出路径 ──
    //
    // `map_out` 靠一个「已占用」集合消解重名，谁先占坑决定谁拿到 `(2)`。
    // 这个顺序若交给并行去定，同一批文件两次跑会得到两套不同的文件名——
    // 交付物命名不可复现，归档时无法解释。所以先串行算完，再并行执行。
    let mut used: HashSet<PathBuf> = HashSet::new();
    let dsts: Vec<Option<PathBuf>> = files
        .iter()
        .map(|(src, rel)| {
            if opts.dry_run {
                None
            } else if opts.in_place {
                Some(src.clone())
            } else {
                opts.out_dir.as_ref().map(|d| map_out(rel, d, &mut used))
            }
        })
        .collect();

    // ── 阶段 3：并行逐文件改写 ──
    let total = files.len();
    let done = AtomicUsize::new(0);
    res.outcomes = parallel_map(&files, threads_used, |i, (src, _rel)| {
        let o = match process_one(
            src,
            dsts[i].as_deref(),
            rules,
            opts.chain,
            opts.dry_run,
            opts.in_place,
            opts.backup,
            opts.longest_first,
            opts.verify_after && !opts.dry_run,
            opts.mirror && !opts.dry_run,
        ) {
            Ok(o) => o,
            Err(e) => Outcome::errored(src, rules, format!("{e:#}")),
        };
        let ordinal = done.fetch_add(1, Ordering::Relaxed) + 1;
        on_file(ordinal, total, &o);
        o
    });

    if opts.rename_files && !opts.dry_run {
        rename_outputs(&mut res.outcomes, rules, opts.chain, &mut res.name_rows);
    }
    if opts.verify_after && !opts.dry_run {
        res.verify_rows = verify_outputs(&res.outcomes, rules, threads_used);
    }
    Ok(res)
}

// ─────────────────────────── 并行 ───────────────────────────

/// 实际使用的线程数：`requested == 0` 表示「自动」，取本机可用并行度。
fn resolve_threads(requested: usize, jobs: usize) -> usize {
    let n = if requested > 0 {
        requested
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    };
    n.max(1).min(jobs.max(1))
}

/// 零依赖的并行映射：把 `items` 按索引分发给若干工作线程，**结果按原顺序返回**。
///
/// ## 为什么手写而不是 rayon
///
/// 离线构建环境里没有 rayon / crossbeam 的缓存，也不值得为一个 `par_iter`
/// 引入新的依赖树。这里要的恰好就是"按索引并行、按索引归位"，
/// 标准库的 `thread::scope` + 原子计数器就够。
///
/// ## 三条必须守住的
///
/// 1. **结果顺序 = 输入顺序**。报告行序若随线程调度抖动，同一批文件两次跑出
///    两份不同的报告，归档时无法解释。所以结果落在与下标一一对应的格子里，
///    收尾时按顺序取出。
/// 2. **单个 item 失败不中断整批**。失败由 `f` 自己包成结果值
///    （[`Outcome::errored`] 的 `ERROR` 状态），并行层不感知错误。
/// 3. `threads == 1` 时**不启线程**，走纯串行路径。这既是排查
///    "是不是并行的锅"的第一手段，也是并行版与串行版做等价对照的开关。
///
/// ## 分配策略
///
/// 用**原子取号**（动态负载均衡）而不是预先把文件平均切给各线程：
/// docx 大小差别很大（同一交付包里 100 KB 与 5 MB 混在一起），
/// 均分会让线程之间进度差出好几倍，取号则谁空谁拿下一个。
fn parallel_map<T, R, F>(items: &[T], requested: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> R + Sync,
{
    let n = items.len();
    let workers = resolve_threads(requested, n);
    if workers <= 1 || n <= 1 {
        return (0..n).map(|i| f(i, &items[i])).collect();
    }

    let next = AtomicUsize::new(0);
    // 每格一把锁：一个文件算完才落一次，锁竞争可以忽略。
    let slots: Vec<Mutex<Option<R>>> = (0..n).map(|_| Mutex::new(None)).collect();

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let r = f(i, &items[i]);
                    // 锁中毒（某个线程 panic）也要把值放进去，否则收尾取不到
                    *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                }
            });
        }
    });

    // 到这里所有工作线程都已 join，每格必然被填过一次。
    slots
        .into_iter()
        .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
        .map(|o| o.expect("每个索引恰好被处理一次"))
        .collect()
}

// ─────────────────────────── 目标收集 ───────────────────────────

fn build_excludes(pats: &[String]) -> Result<globset::GlobSet> {
    let mut b = globset::GlobSetBuilder::new();
    for p in pats {
        b.add(globset::Glob::new(p).with_context(|| format!("排除通配符 `{p}` 非法"))?);
    }
    Ok(b.build().context("排除通配符集合构建失败")?)
}

/// 判断一个文件是否应被排除；返回原因。
fn exclude_reason(path: &Path, ex: &globset::GlobSet, out_dir: Option<&Path>) -> Option<String> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name.starts_with("~$") {
        return Some("Word 临时锁文件".into());
    }
    if name.starts_with(".~lock") {
        return Some("办公软件锁文件".into());
    }
    if let Some(od) = out_dir {
        if let Ok(c) = path.canonicalize() {
            if c.starts_with(od) {
                return Some("位于输出目录内（避免把产物当输入）".into());
            }
        }
    }
    if ex.is_match(path) || (!name.is_empty() && ex.is_match(name)) {
        return Some("命中排除通配符".into());
    }
    None
}

/// 扩展名判定。
fn extension_gate(path: &Path, explicit: bool) -> Option<String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "docx" => None,
        "doc" => Some("老格式 .doc（二进制 OLE，不是 ZIP+XML），本工具不适用，请先另存为 .docx".into()),
        _ => {
            if explicit {
                Some(format!("不是 .docx 文件（后缀 .{ext}）"))
            } else {
                Some(String::new()) // 目录扫描时静默忽略无关文件
            }
        }
    }
}

/// 收集待处理文件：返回 `(绝对路径, 相对路径)` 与跳过清单。
pub fn collect_targets(paths: &[PathBuf], opts: &Options) -> Result<(Vec<(PathBuf, PathBuf)>, Vec<Skipped>)> {
    let ex = build_excludes(&opts.exclude)?;
    let out_canon = opts.out_dir.as_deref().and_then(|d| d.canonicalize().ok());
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut skipped: Vec<Skipped> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for p in paths {
        if p.is_dir() {
            let depth = if opts.recursive { usize::MAX } else { 1 };
            for e in walkdir::WalkDir::new(p).max_depth(depth).follow_links(false) {
                let e = e.with_context(|| format!("遍历目录失败：{}", p.display()))?;
                if !e.file_type().is_file() {
                    continue;
                }
                let path = e.path();
                let rel = path.strip_prefix(p).unwrap_or(path).to_path_buf();
                gate_and_push(
                    path,
                    rel,
                    false,
                    &ex,
                    out_canon.as_deref(),
                    &mut files,
                    &mut skipped,
                    &mut seen,
                )?;
            }
        } else if p.is_file() {
            let name = p
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("out.docx"));
            gate_and_push(
                p,
                name,
                true,
                &ex,
                out_canon.as_deref(),
                &mut files,
                &mut skipped,
                &mut seen,
            )?;
        } else {
            bail!("路径不存在：{}", p.display());
        }
    }

    Ok((files, skipped))
}

/// 通过排除与扩展名两道闸门后，收进待处理清单。
#[allow(clippy::too_many_arguments)]
fn gate_and_push(
    path: &Path,
    rel: PathBuf,
    explicit: bool,
    ex: &globset::GlobSet,
    out_canon: Option<&Path>,
    files: &mut Vec<(PathBuf, PathBuf)>,
    skipped: &mut Vec<Skipped>,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    if let Some(r) = exclude_reason(path, ex, out_canon) {
        skipped.push(Skipped {
            path: path.to_path_buf(),
            reason: r,
        });
        return Ok(());
    }
    if let Some(r) = extension_gate(path, explicit) {
        if !r.is_empty() {
            skipped.push(Skipped {
                path: path.to_path_buf(),
                reason: r,
            });
        }
        return Ok(());
    }
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if seen.insert(key) {
        files.push((path.to_path_buf(), rel));
    }
    Ok(())
}

/// 输出路径映射：保持相对层级，重名时加序号。
fn map_out(rel: &Path, out_dir: &Path, used: &mut HashSet<PathBuf>) -> PathBuf {
    let mut dst = out_dir.join(rel);
    if used.contains(&dst) {
        let stem = rel
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("out")
            .to_string();
        let ext = rel
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("docx")
            .to_string();
        let parent = rel.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let mut n = 2;
        loop {
            dst = out_dir.join(&parent).join(format!("{stem} ({n}).{ext}"));
            if !used.contains(&dst) {
                break;
            }
            n += 1;
        }
    }
    used.insert(dst.clone());
    dst
}

pub fn file_sha(p: &Path) -> Result<String> {
    let bytes = std::fs::read(p).with_context(|| format!("读取失败：{}", p.display()))?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// 处理单个文件。
///
/// `want_verify` 只在**就地替换**下有意义：它决定要不要在覆盖源文件之前
/// 把验证基准（[`verify::PkgSnap`]）扣下来。写副本模式无视它——那条路径的原件
/// 一直在磁盘上，验证阶段按路径读就行。
///
/// `mirror` 也只在**写副本**下有意义（见 [`Options::mirror`]）：一处都没改动时，
/// 是原样复制一份到输出目录（完整镜像），还是什么都不写（默认）。
#[allow(clippy::too_many_arguments)]
pub fn process_one(
    src: &Path,
    dst: Option<&Path>,
    rl: &[Rule],
    chain: bool,
    dry: bool,
    in_place: bool,
    backup: bool,
    longest_first: bool,
    want_verify: bool,
    mirror: bool,
) -> Result<Outcome> {
    let text_parts = package::read_text_parts(src)?;
    let mut replaced: HashMap<String, Vec<u8>> = HashMap::new();
    let mut hits: Vec<Hit> = Vec::new();
    let mut parts: Vec<String> = Vec::new();

    for (name, kind, bytes) in &text_parts {
        let plan = if chain {
            engine::plan_part_chained(name, *kind, bytes, rl)?
        } else {
            engine::plan_part(name, *kind, bytes, rl, longest_first)?
        };
        if plan.changed {
            replaced.insert(name.clone(), plan.new_bytes);
            parts.push(name.clone());
        }
        hits.extend(plan.hits);
    }

    let conflicts = hits.iter().filter(|h| !h.applied).count();
    let applied = hits.iter().filter(|h| h.applied).count();
    let sha_before = file_sha(src)?;

    let meta = std::fs::metadata(src)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format_epoch(d.as_secs()))
        .unwrap_or_default();

    let mut sha_after = sha_before.clone();
    let mut note = String::new();
    let mut verdict: Option<verify::PairVerdict> = None;
    let mut snap_err: Option<String> = None;
    let mut mirrored = false;
    // ★ "本轮真写过盘"的独立记账 —— `produced()` 只看它，不看 `status`。
    // 这样"部分落笔 + 部分冲突"（`status = CONFLICT`）的产物不会被当成"未产出"。
    let mut written = false;

    if !dry && !replaced.is_empty() {
        // ★★ **就地替换的验证基准只能在这里扣下来。**
        //
        // 下面那一步会把 `src` 覆盖掉；覆盖之后，`src` 指的就是产物本身了。
        // 不留基准的话，"执行后自动验证"只能拿这个文件跟它自己比 ——
        // 关卡 1/2 必然全等通过，报表一片绿，实际什么都没验。
        // 骨架快照只在本函数内存活，比完即丢，不跨文件累积。
        let before = if in_place && want_verify {
            match verify::snapshot(src) {
                Ok(s) => Some(s),
                Err(e) => {
                    snap_err = Some(format!("{e:#}"));
                    None
                }
            }
        } else {
            None
        };

        match dst {
            None => {
                note = "未指定输出目录（预览模式），未落盘".into();
            }
            Some(d) => {
                if in_place {
                    // 备份按需生成（默认关）：不勾时源目录一个多余文件都不留。
                    // 勾了则**只在首次**生成——重复跑同一批时，.bak 始终是最初那一版，
                    // 中途被误改也还能回到出发点。
                    let mut bak_note = String::new();
                    if backup {
                        let bak = src.with_extension("docx.bak");
                        if !bak.exists() {
                            std::fs::copy(src, &bak)
                                .with_context(|| format!("写备份失败：{}", bak.display()))?;
                        }
                        bak_note = format!("，备份：{}", bak.display());
                    }
                    // 直接在**原文件上**改写，不再新建 `.wrepl-tmp` 再 rename 顶掉。
                    // 少建一个文件在"新建文件有固定开销"的环境里（实时扫描 / 网络盘 /
                    // 机械盘）省得很实在；代价是失去 rename 的原子性 ——
                    // 保底手段是（勾了备份时的）那份 .bak，以及 rewrite_in_place 内部
                    // 写失败时把内存里的原件写回去。
                    package::rewrite_in_place(src, &replaced)?;
                    note = format!("已就地改写（原文件上直接覆盖）{bak_note}");
                } else {
                    package::write_with_replacements(src, d, &replaced)?;
                }
                // ★ 写盘成功才置位。上面的 `?` 一旦提前返回就走不到这里
                //（那种情况由 `Outcome::errored` 兜底，`written` 保持 false）。
                // 注意：这一支不看 `conflicts`——"部分落笔 + 部分冲突"也是真写了盘，
                // 该由 `written` 如实反映，而不是被 `status` 的冲突语义盖过去。
                written = true;
                sha_after = file_sha(d)?;

                // 写完立刻比、比完就丢。
                if let Some(b) = before {
                    verdict = Some(verify::verify_snap(&b, d));
                } else if let Some(e) = snap_err {
                    note = format!("{note}；执行后验证未做：写盘前取不到基准 —— {e}");
                }
            }
        }
    } else if !dry && mirror && !in_place {
        // ── 完整镜像：没改动，也要在输出目录里出现 ──
        //
        // 走到这里说明一批规则打下来 `replaced` 是空的：一条都没命中（`NO_MATCH`）、
        // 命中了但全是区间冲突没落笔（`CONFLICT`）、或者命中的是"查找==替换"的空转
        // 规则（`NO_CHANGE`）。三种情况文件都没被改动，但勾了完整镜像时输出目录
        // 要能整包拿走，所以原样复制一份过去。
        //
        // 用 `fs::copy` 而不是走 `write_with_replacements`：产物必须与源文件
        // **逐字节相同**——"镜像"这个词的全部内容就是这个。重打包会重排 zip 条目、
        // 换压缩参数，做不到逐字节相同，也就没法用整文件 SHA256 自证复制无损。
        if let Some(d) = dst {
            // ★ 父目录得自己建。写副本那条路（`write_with_replacements`）会按需
            // `create_dir_all`，而 `fs::copy` 不会——不建就是
            // 「系统找不到指定的路径 (os error 3)」。
            //
            // 而且这是个**竞态**：并行跑一批时，`out` 目录由谁先落盘谁创建。
            // 若第一个被处理的恰好是个零命中的文件（它走的就是这条路），
            // 那一刻目录还不存在，整批就冒出一个 ERROR。`create_dir_all` 幂等，
            // 多线程同时建也没事。
            if let Some(parent) = d.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("建输出目录失败：{}", parent.display()))?;
                }
            }
            std::fs::copy(src, d)
                .with_context(|| format!("复制到输出目录失败：{}", d.display()))?;
            written = true;
            sha_after = file_sha(d)?;
            mirrored = true;
            note = "未改动，已原样复制到输出目录（完整镜像）".into();
        }
    }

    let status = if hits.is_empty() {
        "NO_MATCH"
    } else if conflicts > 0 {
        "CONFLICT"
    } else if applied == 0 {
        "NO_CHANGE"
    } else if !replaced.is_empty() && !dry && dst.is_some() {
        "OK"
    } else {
        "DRY_RUN"
    };

    Ok(Outcome {
        src: src.to_path_buf(),
        dst: dst.map(|d| d.to_path_buf()),
        status,
        hits,
        parts,
        conflicts,
        applied,
        note,
        sha_before,
        sha_after,
        size_kb: meta.len() as f64 / 1024.0,
        mtime,
        rule_count: rl.iter().filter(|r| r.enabled).count(),
        verdict,
        mirrored,
        written,
    })
}

/// 把 epoch 秒格式化成 `YYYY-MM-DD HH:MM:SS UTC`（不引第三方日期库）。
/// 把一份运行日志写到指定文件（目录不存在则建）。
///
/// 界面上那块日志是给人当场看的；落盘这份是给归档用的——放在产物目录里，
/// 跟报告挨着，回头找的时候不用回忆。命令行版把同样的东西打到 stdout，
/// 所以这个函数只在界面里被调用，但实现放在内核层：将来命令行要加 `--log`
/// 时直接复用，不另写一份。
pub fn write_run_log(path: &Path, text: &str) -> Result<PathBuf> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).with_context(|| format!("建目录失败 {}", dir.display()))?;
        }
    }
    std::fs::write(path, text).with_context(|| format!("写日志失败 {}", path.display()))?;
    Ok(path.to_path_buf())
}

pub fn format_epoch(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // civil_from_days（Howard Hinnant 算法）
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
}

// ─────────────────────────── 文件名同步改名 ───────────────────────────

/// 输出文件名同步改名。
///
/// 分三阶段，避免"改到一半撞名"的半成品：
/// A 按规则算出全部新名 → B 统一消解重名（含与"未改名文件"的碰撞）→ C 一次性落盘。
///
/// **只改"本次真的落过盘"的文件**（阶段 A 里判 `status == OK`）：写副本模式下没改动的
/// 文件不写进输出目录，产物路径上什么都没有，去 rename 只会报
/// 「系统找不到指定的路径 (os error 3)」；就地替换的 `dst` 就是源文件本身，
/// 未命中也能改名（文件名里带旧编号的文件照样要归一），这条不动。
pub fn rename_outputs(
    outcomes: &mut [Outcome],
    rules: &[Rule],
    chain: bool,
    rows: &mut Vec<NameRow>,
) {
    struct Plan {
        idx: usize,
        old_path: PathBuf,
        old_name: String,
        new_name: String,
        planned: String,
        notes: Vec<String>,
        changed: bool,
        err: Option<String>,
    }

    // ── 阶段 A：算计划 ──
    let mut plans: Vec<Plan> = Vec::new();
    for (i, o) in outcomes.iter().enumerate() {
        if o.status == "ERROR" {
            continue;
        }
        let Some(d) = o.dst.as_ref() else { continue };
        let Some(old_name) = d.file_name().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        match naming::rename_file_name(&old_name, rules, chain) {
            Ok(p) => {
                // ★ 写副本模式有一条设计：**没改动的文件不写进输出目录**（见 README
                // 「落盘位置」）。那种文件在产物路径上根本不存在，改名无处可落 ——
                // 硬去 rename 会报「系统找不到指定的路径 (os error 3)」，看着像
                // 文件名同步功能坏了，其实是"输出目录里压根没有这个文件"。
                //
                // 判据用**本次真的落过盘**（[`Outcome::produced`] = 状态 OK，或勾了
                // 完整镜像时那个"未改动但原样复制"的产物），不用"路径上碰巧有文件"：
                // 输出目录里可能躺着上一轮留下的同名旧文件，那不是本次产物，不该被改名。
                // （就地替换的 `dst` 就是源文件本身，未命中也能改名 —— 文件名里带旧编号
                //   的文件照样要归一，那是既定行为，别在这里误伤。）
                if !o.produced() && d != &o.src {
                    if p.changed() {
                        rows.push(NameRow {
                            index: i + 1,
                            old_name: old_name.clone(),
                            new_name: old_name,
                            changed: false,
                            note: "未产出（无改动，未写输出目录），改名未执行".into(),
                        });
                    }
                    continue;
                }
                let mut notes: Vec<String> = p.warnings.clone();
                if p.conflicts() > 0 {
                    notes.push(format!("{} 处命中区间重叠，冲突未替换", p.conflicts()));
                }
                plans.push(Plan {
                    idx: i,
                    old_path: d.clone(),
                    old_name,
                    new_name: p.new_name.clone(),
                    planned: p.new_name.clone(),
                    notes,
                    changed: p.changed(),
                    err: None,
                });
            }
            Err(e) => rows.push(NameRow {
                index: i + 1,
                old_name: old_name.clone(),
                new_name: old_name,
                changed: false,
                note: format!("改名未执行：{e:#}"),
            }),
        }
    }

    // ── 阶段 B：消解重名 ──
    let untouched: HashSet<String> = outcomes
        .iter()
        .enumerate()
        .filter(|(i, _)| !plans.iter().any(|p| p.idx == *i))
        .filter_map(|(_, o)| o.dst.as_ref())
        .filter_map(|d| d.file_name().and_then(|s| s.to_str()))
        .map(|s| s.to_lowercase())
        .collect();

    let mut taken: HashSet<String> = untouched;
    for p in plans.iter_mut() {
        if !p.changed {
            taken.insert(p.new_name.to_lowercase());
            continue;
        }
        let mut cand = p.new_name.clone();
        let mut n = 2;
        while taken.contains(&cand.to_lowercase())
            || (cand != p.old_name && p.old_path.with_file_name(&cand).exists())
        {
            let (stem, ext) = split_stem_ext(&cand);
            cand = format!("{stem} ({n}){ext}");
            n += 1;
            if n > 500 {
                break;
            }
        }
        if cand != p.new_name {
            p.notes.push(format!(
                "目标文件名已被占用，自动加序号：`{}` → `{cand}`",
                p.new_name
            ));
            p.new_name = cand;
        }
        taken.insert(p.new_name.to_lowercase());
    }

    // ── 阶段 C：落盘 ──
    for p in plans.iter_mut() {
        if !p.changed || p.new_name == p.old_name {
            continue;
        }
        let target = p.old_path.with_file_name(&p.new_name);
        match std::fs::rename(&p.old_path, &target) {
            Ok(()) => {
                // 就地替换时 `src` 与 `dst` 指的是**同一个物理文件**：改了名，
                // 两边都得跟着走。只更新 dst 的话，报告里的"文件路径"就指向一个
                // 已经不存在的旧名字——事后按报告去核对会一头雾水。
                if outcomes[p.idx].src == p.old_path {
                    outcomes[p.idx].src = target.clone();
                }
                outcomes[p.idx].dst = Some(target);
            }
            Err(e) => p.err = Some(format!("{e}")),
        }
    }

    for p in &plans {
        let note = match &p.err {
            Some(m) => format!("改名落盘失败：{m}"),
            None => p.notes.join("；"),
        };
        rows.push(NameRow {
            index: p.idx + 1,
            old_name: p.old_name.clone(),
            new_name: if p.err.is_some() {
                p.old_name.clone()
            } else {
                p.planned.clone()
            },
            changed: p.changed && p.err.is_none(),
            note,
        });
    }
}

/// 拆词干与扩展名（与 naming 模块同一规则：以点开头视为无扩展名）。
fn split_stem_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

// ─────────────────────────── 执行后验证 ───────────────────────────

/// 执行后自动验证：对每个产物跑关卡 1+2，并做全包残留自检。
///
/// ## 关卡 1/2 的左侧基准从哪来
///
/// - **写副本**：原件还在磁盘上（`o.src`），按路径现读现比。
/// - **就地替换**：原件已被覆盖，`o.src` 现在指的就是产物本身（改过名的话连这个
///   路径都失效了）。基准是 `process_one` 在写盘前扣下、写盘后当场比完的
///   `o.verdict`——这里直接取用，**不再去读任何"原件路径"**。
///
/// 残留自检两条路都要做：它只看落盘后的那个文件（没产出时退回源文件），与基准无关。
///
/// ## 没产出的文件：输出目录里根本没有它，这是设计
///
/// 写副本模式有一条刻意的设计——**没改动的文件不写进输出目录**（`--out` 里只有本次
/// 真正改过的那些，见 README「落盘位置」与回归第 10 组）。早先这里对这些文件照样
/// 拿 `(源文件, 产物路径)` 去比对，而那个产物路径压根不存在，于是整批报
/// `残留自检未能执行：打不开文件：…/out/…docx: 系统找不到指定的文件 (os error 2)`。
///
/// 正确做法：**没有产物，就没有"产物 vs 源文件"可比**。此时把自检对象退回源文件
/// （"未改动"这件事的全部内容就在源文件里），并在备注里写明「未产出」。这样同一个
/// 文件的验证结论不随落盘模式漂移——就地模式下没改动的文件同样是这么验的（它也没
/// 被写过，也是"拿源文件做自检"）。
///
/// 顺带盯一个陷阱：输出目录里**可能躺着上一轮留下的同名旧文件**（这轮规则变了、
/// 不再命中它）。那不是本次产物，绝不能拿去验证；它也不会被覆盖（"不覆盖已存在
/// 文件"是既定策略），所以只如实写进备注提醒。
pub fn verify_outputs(outcomes: &[Outcome], rules: &[Rule], threads: usize) -> Vec<VerifyRow> {
    /// 待验证对象：产物路径 + 「这一轮到底产出了没有」。
    struct Job {
        /// 就地路径在写盘前扣下的裁决（写副本模式为 `None`，要按路径现算）。
        pre: Option<verify::PairVerdict>,
        src: PathBuf,
        dst: PathBuf,
        /// 本次运行**真的写出了** `dst`（见 [`Outcome::produced`]）。
        produced: bool,
        /// 本次是"未改动 → 原样复制"（完整镜像）。这类产物与源文件逐字节相同，
        /// 关卡 1/2 不必读包，比整文件 SHA256 即可，见下面 `verdicts` 那段。
        mirrored: bool,
        /// 整文件 SHA256（复制前后的对照，只对镜像产物用）。
        sha_before: String,
        sha_after: String,
        /// 就地替换：`dst` 与 `src` 是同一个物理文件，备注措辞要跟着变。
        in_place: bool,
    }

    // 顺序 = outcomes 顺序（报告行序必须稳定）。
    let jobs: Vec<Job> = outcomes
        .iter()
        .filter(|o| o.status != "ERROR")
        .filter_map(|o| {
            o.dst.clone().map(|d| Job {
                pre: o.verdict.clone(),
                in_place: d == o.src,
                src: o.src.clone(),
                dst: d,
                produced: o.produced(),
                mirrored: o.mirrored,
                sha_before: o.sha_before.clone(),
                sha_after: o.sha_after.clone(),
            })
        })
        .collect();

    // 已在 process_one 里验过的（就地路径）不必重算——它的"原件路径"根本不存在了。
    // 写副本路径的原件还在盘上，这里按路径现算。两条路的判据都在 verify 里，只有一份。
    // **没产出的不验**：文件都没写出来，谈不上"格式有没有被动"。
    //
    // 镜像产物单独走一条快捷路：它是 `fs::copy` 出来的，与源文件**逐字节相同**，
    // 拿两个包去比关卡 1/2 必然全等——这个结论毫无信息量，却要为此把两个 docx
    // 各解压读一遍（完整镜像下未命中的文件通常是多数，这一步会很贵）。改成直接比
    // 整文件 SHA256：相等就证明复制无损，**比逐 part 比对更强**，而且零额外 IO。
    let verdicts: Vec<Option<verify::PairVerdict>> =
        parallel_map(&jobs, threads, |_i, j| match &j.pre {
            Some(v) => Some(v.clone()),
            None if j.mirrored => {
                let same = !j.sha_before.is_empty() && j.sha_before == j.sha_after;
                Some(verify::PairVerdict {
                    src: j.src.clone(),
                    dst: j.dst.clone(),
                    l1_pass: same,
                    l2_pass: same,
                    changed_parts: Vec::new(),
                    note: if same {
                        "未改动，已原样复制到输出目录（整文件逐字节一致）".into()
                    } else {
                        "未改动，但复制后的字节与原文件不一致（复制过程有问题）".into()
                    },
                })
            }
            None if j.produced => Some(verify::verify_one(&j.src, &j.dst)),
            None => None,
        });

    parallel_map(&jobs, threads, |i, j| {
        let v = &verdicts[i];
        // 文件名与残留自检都走**当前**的产物路径；没产出就退回源文件。
        // 不能用 `v.dst`：就地路径的裁决是写盘那一刻算的，那之后还可能改名。
        let file = j
            .dst
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let target: &Path = if j.produced { &j.dst } else { &j.src };
        let (l1, l2, mut note) = match v {
            Some(v) => (
                if v.l1_pass { "通过" } else { "不通过" }.to_string(),
                if v.l2_pass { "通过" } else { "不通过" }.to_string(),
                v.note.clone(),
            ),
            // 未产出：不存在"产物 vs 源文件"这回事，格式也就谈不上被动过。
            // 但**必须写明**——只看到一片"通过"会让人以为输出目录里有这个文件。
            None => (
                "通过".to_string(),
                "通过".to_string(),
                if j.in_place {
                    "未改动（源文件未写盘）".to_string()
                } else {
                    "未产出（无改动，未写输出目录）".to_string()
                },
            ),
        };
        if !j.produced {
            note.push_str("；残留自检按源文件做");
            if !j.in_place && j.dst.exists() {
                // 上一轮留下的旧产物：同名，但不是这次写出来的，不能当成产物看。
                note.push_str("；输出目录里已有同名旧文件，本次未覆盖（未参与验证）");
            }
        }

        let res = match verify::residue(target, rules) {
            Ok(r) => r,
            Err(e) => {
                return VerifyRow {
                    file,
                    level1: l1,
                    level2: l2,
                    residue: "自检失败".to_string(),
                    note: if j.produced {
                        format!("残留自检未能执行：{e:#}")
                    } else {
                        format!("残留自检未能执行（按源文件）：{e:#}")
                    },
                };
            }
        };

        let (residue, res_note) = if res.clean() {
            ("无残留".to_string(), String::new())
        } else {
            let text = res.text_total();
            (
                // 总处数里绝大部分是 XML 标记/属性里的字符（rsid 十六进制、修订作者……），
                // 所以必须把"落在可见文本里"的处数一起报出来，否则数字看着吓人却找不到病根。
                format!("残留 {} 处（其中可见文本 {} 处）", res.total(), text),
                res.entries
                    .iter()
                    // 已按处数降序（见 verify::residue），这里取前 4 条就是最该看的那几条。
                    .take(4)
                    .map(|e| {
                        // 一律标出"可见文本"处数：`0` 就说明这条规则的残留
                        // 全落在 XML 标记/属性里，不是正文漏改。
                        format!(
                            "规则#{}（`{}`）×{}（可见文本 {}）",
                            e.rule_id, e.needle, e.count, e.in_text
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("；"),
            )
        };

        if !res_note.is_empty() {
            if !note.is_empty() {
                note.push('　');
            }
            note.push_str(&res_note);
            if j.mirrored {
                // 镜像产物是"未改动"的文件，旧串本来就在里面，报残留是**正常的**。
                // 不写这一句，读报告的人会把一整批没改过的文件当成"没替换干净"。
                note.push_str("（该文件未改动，这些旧串本就是原文件内容）");
            }
        }
        if note.is_empty() {
            // 能走到这里只可能是"验过、但既没备注也没残留"（未产出那几行上面已填过字）。
            if let Some(v) = v {
                note = format!("改动 part：{}", v.changed_parts.join(", "));
            }
        }

        VerifyRow {
            file,
            level1: l1,
            level2: l2,
            residue,
            note,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_format_is_utc_shaped() {
        // 2026-09-24 00:00:00 UTC
        assert_eq!(format_epoch(1790208000), "2026-09-24 00:00:00 UTC");
    }

    #[test]
    fn splits_stem_and_ext() {
        assert_eq!(split_stem_ext("a.docx"), ("a", ".docx"));
        assert_eq!(split_stem_ext("a.b.docx"), ("a.b", ".docx"));
        assert_eq!(split_stem_ext(".gitignore"), (".gitignore", ""));
        assert_eq!(split_stem_ext("noext"), ("noext", ""));
    }

    #[test]
    fn excludes_word_lock_files() {
        let ex = build_excludes(&[]).unwrap();
        let p = PathBuf::from("/tmp/~$文件一.docx");
        assert!(exclude_reason(&p, &ex, None).is_some());
        let ok = PathBuf::from("/tmp/正常文件.docx");
        assert!(exclude_reason(&ok, &ex, None).is_none());
    }

    #[test]
    fn doc_is_gated_out_with_reason() {
        let r = extension_gate(&PathBuf::from("a.doc"), false).unwrap();
        assert!(r.contains("老格式"));
        assert!(extension_gate(&PathBuf::from("a.docx"), false).is_none());
    }
}
