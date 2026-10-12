//! 页眉替换 —— **纯 Rust 改 OOXML，不依赖 Word**。
//!
//! ## 为什么不再走 Word COM
//!
//! 早期版本把「页眉替换（文本 / 图片）」交给了 Word 的自动化接口（`wordtool::services`）。
//! 那条路有两个躲不开的代价：
//!
//! 1. **目标机器必须装 Word**。而同一个程序里的「批量替换」是纯 Rust 改 OOXML，
//!    在任何机器上都能跑 —— 一个工具里两套前提，用户很难记；
//! 2. **失败时看不出来**。COM 那边"打开→改→保存"任何一步没落地，结果往往只是一句
//!    笼统的"处理出错"，甚至界面报成功、源文件一个字节没变。
//!
//! 现在这一页与「批量替换」共用同一条原理：**只改该改的字节**。
//!
//! ## 改什么（与 COM 版逐字段等价）
//!
//! 对文档里**每一个被节引用到的页眉**（`w:headerReference`，`default` / `first` /
//! `even` 三种类型都算）里**第一个表格的第一个单元格**：
//!
//! * 文本：清空该格，写「英文 + 手动换行 + 中文」（只有中文时纯中文），
//!   字体固定 `Times New Roman` / 东亚 `宋体` / 9pt（小五）/ 不粗不斜无下划线；
//! * 图片：清空该格，插入图片，按表格行高等比缩放（可关），格子内水平 + 垂直居中。
//!
//! 被改掉的 COM 版**只动主页眉**（`wdHeaderFooterPrimary`）。这里三种类型全改：
//! 同一个模板往往"首页竖版、内页横版"，或者标题页单独一张表，只改主页眉等于
//! 交出去的文件里还留着旧抬头 —— 那正是最难在验收时发现的一类错。
//!
//! 三种类型各自**独立继承**（见下节），凡是"被引用到"的部件都改；
//! 包里存在却没有任何节引用的页眉部件**不动**，但会在日志里报出来。
//!
//! ## 三条实现上的硬约束
//!
//! * **不重建 XML**。所有改动都是"在原始字节流上定点替换"，其余字节一个不碰 ——
//!   属性顺序、命名空间前缀、缩进全部原样保留（同 [`crate::docx::rewrite`] 的做法）。
//! * **偏移只能靠扫描得出**，绝不靠"标签长度 = 常量 + 名字长度"这类算式：
//!   XML 前缀可以是任意长度（`<x:tbl>` 也合法），少算一个字节就是一份坏文档。
//!   所以扫描器返回的是**完整区间**（[`Block`]），不是长度。
//! * **生成片段自带命名空间声明**。Word 并不把 `xmlns:a` / `xmlns:pic` 声明在根上，
//!   而是**直接写在用到它的那个元素上**（实测：`<a:graphic xmlns:a="…">`）。
//!   所以本模块生成的 `w:drawing` 也把 `wp` / `a` / `pic` / `r` 全挂在自己身上 ——
//!   不管目标页眉的根元素声明了什么，片段都是合法 XML。
//!
//! ## 多节 + 横竖版混排（这是最容易做漏的一块）
//!
//! 一份交付文档常常是"封面竖版 + 表格横版"这样混着来的，也就是**多个节**，
//! 而且各节的页面方向、页边距都可能不同。COM 版靠 Word 的 `Section.Headers`
//! 逐个节去问，自己不操心；换成纯 Rust 就得自己把"节"的语义补齐：
//!
//! * **逐节扫 `w:sectPr`**：节的先后顺序 = `w:sectPr` 在 `document.xml` 里的字节顺序
//!   （段落里的 sectPr 表示"这一节到此为止"，正文末尾那个是最后一节）。
//! * **三种类型各自独立继承**：某节没写 `w:type="first"` 的引用时，它的首页眉沿用
//!   **前一节**的首页眉；`default` / `even` 同理、互不干扰。漏了这条，多节文档里
//!   就会少改几处 —— 而"少改"恰恰是最难发现的一类错。
//! * **去重按部件、不按节**：几个节共用同一个页眉部件是常态。不去重就会对同一处改
//!   两遍（第二遍是在已改过的内容上再定位），结果是白做功，甚至把上次插的图再插一张。
//! * **横竖版不需要特殊处理，但必须用对基准**：图片尺寸取自**该页眉自己的表格**
//!   （首列 `w:gridCol` 宽 / 首行 `w:trHeight` 高），而表格就住在页眉部件里 ——
//!   横版节那张表本来就比竖版宽，各算各的即可；拿页面宽度去算才是错的。
//! * **不看 `w:titlePg` / `evenAndOddHeaders`**：这两个开关只决定"当前是否显示"，
//!   不影响该不该把内容改对。一律改掉，将来谁把这个开关打开，抬头都是新的。
//! * **孤立的页眉部件不碰**：包里存在、但没有任何节引用的 `word/headerN.xml`
//!   一律不改（那些是编辑历史的残留），但会在日志里明确列出来 ——
//!   "哪些没被改到"必须让人看见，而不是让人对着屏幕猜。
//!
//! ## 明确不支持：`.doc`
//!
//! Word 97-2003 的 `.doc` 是 OLE 二进制综合体，不是 zip，纯 Rust 读不了。
//! 遇到就**明确报错**（提示另存为 `.docx`），不做静默跳过 —— 静默正是 COM 时代
//! 最坑人的地方。
//!
//! ## 落盘：两种，由调用方选（见 [`Out`]）
//!
//! * [`Out::SiblingOut`] —— 写到每份源文件旁的 `out/` 子目录，源文件一个字节不动。
//!   **图形界面上的默认就是它**：这一页处理的是交付件，先拿副本来验证
//!   比事后恢复便宜得多。
//! * [`Out::InPlace`]（enum 的 `Default`，因为它与 COM 版行为一致）—— 就地覆盖源文件。
//!   这是**唯一**能弄坏源文件的路径，所以它有一道真兜底：写完之后跑 [`self_check`]，
//!   不过就把**原件字节写回去**再报错。也就是说最坏结果是"这次没做成 + 一条能看懂的
//!   错"，而不是"源文件坏了"。
//!
//! 界面上选 `InPlace` 会先弹一次确认框（见 `wordtool::ui`）—— 库这一层不管确认，
//! 它只负责"说到做到 + 出错回滚"。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::docx::package::{self, PartKind};

// ─────────────────────────── 对外接口 ───────────────────────────

/// 要对页眉做的那一件事。
#[derive(Clone, Debug)]
pub enum Action {
    /// 文本替换：`en` 在上、`cn` 在下，中间是 Word 手动换行符。
    Text { en: String, cn: String },
    /// 图片替换：`scale_to_table` 为真时按表格行高等比缩放（同 COM 版的复选框）。
    Image {
        path: PathBuf,
        scale_to_table: bool,
    },
}

/// 落盘策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Out {
    /// 就地覆盖源文件（默认）—— 与 COM 版的行为一致。
    #[default]
    InPlace,
    /// 写到每份源文件**各自所在目录**下的 `out/` 子目录，**源文件一个字节不动**。
    ///
    /// 用"各自的目录"而不是一个统一的输出目录：自绘选择器允许一次挑进好几个
    /// 文件夹里的文件，统一目录必然撞名 —— 而撞名在批处理里就是"悄悄覆盖"。
    SiblingOut,
}

/// 在 `dir` 下挑一个不撞名的落点：`a.docx` → `a (2).docx` → `a (3).docx` …
///
/// 与「批量替换」页同一口径：**绝不悄悄覆盖输出目录里已有的文件**。
fn unique_in_dir(dir: &Path, name: &str) -> PathBuf {
    let picked = pick_free_name(name, |n| dir.join(n).exists());
    dir.join(picked)
}

/// [`unique_in_dir`] 的纯函数部分：`taken(n)` 说名字 `n` 是否已被占用。
///
/// 拆出来是为了**能测** —— 落点命名规则一旦写错，后果是"悄悄覆盖了上次的产物"，
/// 这种错必须靠断言挡在测试里，不能靠"跑一遍看看"。
fn pick_free_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    // 按**最后一个点**切扩展名（`a.b.docx` 的扩展名是 `.docx`）；
    // 名字以点开头（`.gitignore`）或压根没点时，整套名字当主干、不带扩展名。
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    for n in 2..1000u32 {
        let cand = format!("{stem} ({n}){ext}");
        if !taken(&cand) {
            return cand;
        }
    }
    name.to_string()
}

/// 跑一批。返回 `(成功数, 失败数)`。
///
/// 一个文件失败不影响其它文件（与 COM 版的批处理语义一致）。
///
/// **"页眉里没有表格"算失败**，不算成功、也不静默跳过：从用户角度看，
/// "这份文件我没改到"就是这次操作没达成。把它记成成功，正是 COM 时代
/// 让人白查一下午的那类谎话。
pub fn run(
    docs: &[String],
    action: &Action,
    out: Out,
    log: &mut dyn FnMut(String),
    progress: &mut dyn FnMut(usize, usize),
) -> (i32, i32) {
    let total = docs.len();

    // 图片只读一次：一批几十份文档用同一张图，没必要每份都去读盘 + 判格式。
    // 读不了就整批不做 —— 让用户在**第一条**日志里就看到原因，而不是"失败 N 份"。
    let img = match action {
        Action::Image { path, .. } => match ImageBytes::load(path) {
            Ok(i) => Some(i),
            Err(e) => {
                log(format!("读取图片失败：{e:#}"));
                return (0, total as i32);
            }
        },
        Action::Text { .. } => None,
    };

    let (mut ok, mut fail) = (0i32, 0i32);
    for (i, d) in docs.iter().enumerate() {
        progress(i + 1, total);
        let src = PathBuf::from(d);
        let name = file_name(d).to_string();

        let (dst, in_place) = match out {
            Out::InPlace => (src.clone(), true),
            Out::SiblingOut => {
                // 源文件没有父目录（只可能是裸文件名）时落到当前目录，与"旁"一个意思
                let dir = src.parent().unwrap_or_else(|| Path::new(".")).join("out");
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    fail += 1;
                    log(format!(
                        "建不了输出目录，跳过：{name}（{}：{e}）",
                        dir.display()
                    ));
                    continue;
                }
                let dst = unique_in_dir(&dir, &name);
                if dst.file_name().map(|f| f != name.as_str()).unwrap_or(false) {
                    log(format!(
                        "输出目录里已有同名文件，这一份改名为 {}：{name}",
                        dst.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
                (dst, false)
            }
        };

        match process_one(&src, &dst, in_place, action, img.as_ref()) {
            Ok(note) => {
                ok += 1;
                if in_place {
                    log(format!("{note}：{name}"));
                } else {
                    log(format!("{note}：{name} → {}", dst.display()));
                }
            }
            Err(e) => {
                fail += 1;
                log(format!("处理出错，跳过：{name}（{e:#}）"));
            }
        }
    }
    (ok, fail)
}

fn file_name(p: &str) -> &str {
    p.rsplit(['\\', '/']).next().unwrap_or(p)
}

// ─────────────────────────── 单文件处理 ───────────────────────────

/// 处理一份文档，返回给人看的一句话（成功后接在日志里）。
fn process_one(
    src: &Path,
    dst: &Path,
    in_place: bool,
    action: &Action,
    img: Option<&ImageBytes>,
) -> Result<String> {
    if src
        .extension()
        .map(|e| e.eq_ignore_ascii_case("doc"))
        .unwrap_or(false)
    {
        bail!(
            "`.doc`（Word 97-2003 二进制格式）不是 zip 容器，纯 Rust 读不了；\
             请先在 Word 里「另存为」成 .docx 再替换"
        );
    }

    // 一次开容器，只解压这一趟真正要看的部件：正文（拿节与页眉引用）、关系表、
    // 各页眉本身、以及图片模式要改的内容类型表。**不碰 `word/media/`** ——
    // 那些动辄几十 MB，为了拿一份名单去解压它们纯属白干。
    let parts = package::read_parts_where(src, |name, kind| {
        name == "word/document.xml"
            || name == "[Content_Types].xml"
            || name == "word/_rels/document.xml.rels"
            || kind == PartKind::Header
            || (name.starts_with("word/_rels/header") && name.ends_with(".rels"))
    })?;
    let get = |n: &str| {
        parts
            .iter()
            .find(|(name, _, _)| name == n)
            .map(|(_, _, b)| b.as_slice().to_vec())
    };

    let doc_xml = get("word/document.xml")
        .context("docx 里没有 word/document.xml —— 容器结构异常")?;
    let doc_rels = get("word/_rels/document.xml.rels").unwrap_or_default();
    // 逐节算出**所有被引用到的页眉部件**：`default` / `first` / `even` 三种类型全要，
    // 三种类型各自独立继承前一节。横竖版混排不影响这一步 —— 页面方向写在 `w:pgSz` 上，
    // 与页眉引用无关；尺寸只跟页眉自己的表格走（见 `locate_first_cell`）。
    let sh = section_headers(&doc_xml, &parse_rels(&doc_rels));
    if sh.refs.is_empty() {
        bail!(
            "这一份文档没有任何页眉引用（各节的 `w:sectPr` 里都没有 `w:headerReference`）\
             —— 没有可替换的位置"
        );
    }
    let targets = sh.parts();

    // 已存在的部件名（**只列名、不解压**）—— 新增媒体部件前靠它避免撞名。
    let existing: Vec<String> = package::part_names(src)?;

    // 包里存在、但没有任何节引用的页眉部件：那是编辑历史的残留，本工具不碰，
    // 但要如实列出来 —— 否则用户会以为"这份文件里所有抬头都换过了"。
    let orphans: Vec<String> = existing
        .iter()
        .filter(|n| is_header_part(n) && !targets.contains(n))
        .cloned()
        .collect();

    let mut replaced: HashMap<String, Vec<u8>> = HashMap::new();
    let mut added: Vec<(String, Vec<u8>)> = Vec::new();
    let mut done = 0usize;
    let mut no_table: Vec<String> = Vec::new();

    // 整份文档**共用一份媒体部件**：多个节的页眉要放同一张图时，图片字节只进包一次
    // （各页眉的 rels 各自指过来，缩放尺寸写在各自的 `wp:extent` 里）。
    let media = match (action, img) {
        (Action::Image { .. }, Some(i)) => Some(alloc_media_name(&existing, i.ext)),
        _ => None,
    };
    let mut media_used = false;

    for tname in &targets {
        // 引用的部件在包里不存在（坏包，或 rels 指错了）—— 记一笔，不当成"改成功"。
        let Some(hdr) = get(tname) else {
            no_table.push(format!("{tname}（部件缺失）"));
            continue;
        };
        let Some(cell) = locate_first_cell(&hdr) else {
            no_table.push(tname.clone());
            continue;
        };

        let (ins, side) = match action {
            Action::Text { en, cn } => (
                text_cell_xml(&hdr, &cell, en, cn),
                Vec::new(),
            ),
            Action::Image {
                scale_to_table, ..
            } => {
                let img = img.context("图片模式必然已读入图片")?;
                let media_name = media.as_deref().context("图片模式必然已分配媒体名")?;
                // ① 关系表：拿到（或新建）指向图片的 rId。关系表是**另一个部件**，
                //    它的改动走 `replaced`，不是页眉 XML 上的编辑。
                let rels_name = part_rels_name(tname);
                let (rid, new_rels) = ensure_image_rel(get(&rels_name).as_deref(), media_name);
                if let Some(bytes) = new_rels {
                    replaced.insert(rels_name, bytes);
                }
                media_used = true;
                // ② 页眉 XML：单元格内容 + 表格属性（锁列 / 行高）
                image_cell_edits(&hdr, &cell, img, *scale_to_table, &rid)?
            }
        };

        let mut eds = vec![Ed {
            at: cell.open_end,
            del: cell.close_start - cell.open_end,
            ins: ins.into_bytes(),
        }];
        eds.extend(side);
        replaced.insert(tname.clone(), apply_edits(&hdr, eds));
        done += 1;
    }

    // 一处都没改成 = 这次操作没达成。**算失败**，不算成功 —— 从用户角度看，
    // "这份文件我没改到"就是没做成。把它记成成功，正是 COM 时代最坑人的地方。
    if done == 0 {
        bail!(
            "被引用的页眉里都没有表格（{}）—— 没有可替换的位置",
            no_table.join("、")
        );
    }

    // 图片确实要进包，才动内容类型表与媒体条目。
    if media_used {
        if let (Some(name), Some(i)) = (media.as_deref(), img) {
            if let Some(ct) = get("[Content_Types].xml") {
                let ct2 = ensure_default_ext(&ct, i.ext, i.content_type);
                if ct2 != ct {
                    replaced.insert("[Content_Types].xml".to_string(), ct2);
                }
            }
            added.push((name.to_string(), i.bytes.clone()));
        }
    }

    // ── 落盘 ──
    //
    // 就地覆盖要多留一份原件字节：一是 `rewrite_in_place_with_ext` 需要它来在内存里
    // 拼新包（zip 读是惰性的，不能边读边写同一个文件），二是下面自检不过时要拿它
    // 把源文件写回去。
    let note = if in_place {
        let orig = std::fs::read(src).with_context(|| format!("读取失败：{}", src.display()))?;
        package::rewrite_in_place_with_ext(src, &replaced, &added, &orig)?;
        if let Err(e) = self_check(src, &replaced, &added, action, media.as_deref()) {
            // 自检不通过 = 产物可能不合格。**把原件写回去**再如实报错 ——
            // 留下一份坏掉的源文件，比"这次没做成"严重得多。
            return match std::fs::write(src, &orig) {
                Ok(()) => Err(e).with_context(|| {
                    format!(
                        "自检未通过，已把原件写回 {}（源文件应保持原样；修好原因后重跑即可）",
                        src.display()
                    )
                }),
                Err(rb) => Err(e).with_context(|| {
                    format!(
                        "自检未通过，且**把原件写回也失败**（{rb}）——\
                         该源文件可能已损坏，请用备份或版本控制里的版本恢复"
                    )
                }),
            };
        }
        format!("已就地改写 {done} 个页眉")
    } else {
        package::write_with_replacements_ext(src, dst, &replaced, &added)?;
        self_check(dst, &replaced, &added, action, media.as_deref())?;
        format!("已写出 {done} 个页眉")
    };

    // 备注：把"看上去该改却没改"的地方如实列出来，别让人以为全改干净了。
    //
    // 这里只讲**页眉**。页脚（`w:footerReference` → `word/footerN.xml`）本工具
    // 一个字都不碰 —— 那不是"漏改"，是不在范围内，说清楚的地方在界面文案里。
    let mut notes: Vec<String> = Vec::new();
    if !no_table.is_empty() {
        notes.push(format!(
            "其中 {} 个页眉里没有表格，未处理：{}",
            no_table.len(),
            no_table.join("、")
        ));
    }
    if !orphans.is_empty() {
        notes.push(format!(
            "包内另有 {} 个页眉部件不被任何节引用，未处理：{}",
            orphans.len(),
            orphans.join("、")
        ));
    }
    let scope = format!(
        "本文档引用到 {} 个页眉：{}",
        sh.refs.len(),
        sh.describe()
    );
    if notes.is_empty() {
        Ok(format!("{note}；{scope}"))
    } else {
        Ok(format!("{note}；{scope}；{}", notes.join("；")))
    }
}

// ─────────────────────────── 自检 ───────────────────────────

/// 产物自检：**不复用 `verify` 关卡 1/2**。
///
/// 那两关是围绕"唯一合法改动面是 `w:t` 文本"建的（关卡 2 直接比 XML 骨架），
/// 而页眉替换**本来就要动结构**（清格、写新 run、插 drawing、加媒体部件）——
/// 套上去必然报假警。所以这里按本功能的实际约定另立三条：
///
/// 1. **计划内的部件确实写进去了**：逐字节比对「我打算写的」与「产物里的」；
/// 2. **新增部件真的在包里**（媒体图）；
/// 3. **图片接线是通的**：页眉里的 `r:embed` 所指的 rId，在它自己的关系表里
///    必须指向本次新增的那张图 —— 这一条专抓"关系表没接上"这类坏包
///    （Word 打开会直接报错，而字节层面看不出问题）。
///
/// 容器完整性由 [`package::inspect`] 顺带保证：它会把每个条目都解压出来算 SHA。
fn self_check(
    dst: &Path,
    replaced: &HashMap<String, Vec<u8>>,
    added: &[(String, Vec<u8>)],
    action: &Action,
    media: Option<&str>,
) -> Result<()> {
    // ① 容器完整：`inspect` 会把每个条目解压出来算 SHA256 —— 读得通才谈得上别的。
    let infos = package::inspect(dst).context("自检：产物不是完好的 docx 容器")?;

    // ② 计划内的部件确实写进去了（逐字节比）
    for (name, want) in replaced {
        let got = package::read_part(dst, name)
            .with_context(|| format!("自检：产物里读不到 {name}"))?;
        if &got != want {
            bail!("自检：{name} 的内容与预期不一致（写入似乎被截断）");
        }
    }
    // ③ 新增部件真的在目录里、内容也对
    for (name, want) in added {
        let got =
            package::read_part(dst, name).with_context(|| format!("自检：产物里没有 {name}"))?;
        if &got != want {
            bail!("自检：{name} 的内容与预期不一致");
        }
        if !infos.iter().any(|p| &p.name == name) {
            bail!("自检：{name} 没进 zip 目录");
        }
    }

    // ④ 目标页眉里能查到本次的标记
    for name in replaced.keys().filter(|n| n.starts_with("word/header")) {
        let xml = package::read_part(dst, name)?;
        let xml = String::from_utf8_lossy(&xml).into_owned();
        match action {
            Action::Text { .. } => {
                if !xml.contains(r#"w:eastAsia="宋体""#) {
                    bail!("自检：{name} 里没找到本次写入的字体标记");
                }
            }
            Action::Image { .. } => {
                if !xml.contains("<w:drawing>") {
                    bail!("自检：{name} 里没找到插入的图片");
                }
                let rid = embed_rid(&xml)
                    .with_context(|| format!("自检：{name} 里的图片没有 r:embed 引用"))?;
                let Some(media) = media else {
                    bail!("自检：图片模式却没有媒体部件名")
                };
                // 关系表里这个 rId 必须指向本次新增的媒体部件
                let rels_name = part_rels_name(name);
                let rels = package::read_part(dst, &rels_name)
                    .with_context(|| format!("自检：产物里读不到 {rels_name}"))?;
                match parse_rels(&rels).get(&rid) {
                    Some(t) if t == media => {}
                    Some(t) => bail!("自检：{name} 的图片指向 {t}，不是本次新增的 {media}"),
                    None => bail!("自检：{name} 引用了 {rid}，但关系表里没有这条记录"),
                }
            }
        }
    }
    Ok(())
}

/// 从页眉 XML 里取出图片的 `r:embed` 值（取第一个）。
fn embed_rid(xml: &str) -> Option<String> {
    let b = xml.as_bytes();
    let mut from = 0usize;
    while let Some(t) = find_open(b, from, "blip") {
        if let Some(v) = attr_of(&b[t.start..t.open_end], ":embed") {
            return Some(v);
        }
        from = t.open_end;
    }
    None
}

// ─────────────────────────── 图片解码 ───────────────────────────

/// 读进来的一张图片：原始字节 + 格式 + **原始像素尺寸**。
///
/// 像素尺寸是纯 Rust 方案比 COM 版省事的地方：COM 得先把图片插进文档、再回头量
/// `InlineShape.Width/Height`，而 PNG/JPEG 的文件头里本来就写着宽高。
struct ImageBytes {
    bytes: Vec<u8>,
    /// 扩展名（小写，决定媒体部件名与内容类型）
    ext: &'static str,
    content_type: &'static str,
    px_w: u32,
    px_h: u32,
    /// 用于 Word「选择窗格」里显示的名字（取原文件名）
    label: String,
}

/// 1 磅 = 12700 EMU；96 DPI 下 1 像素 = 914400/96 = 9525 EMU；1 twip(1/20 磅) = 635 EMU。
const EMU_PER_PX: i64 = 9525;
const EMU_PER_TWIP: i64 = 635;

impl ImageBytes {
    fn load(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("读取图片失败：{}", path.display()))?;
        let label = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("页眉图片")
            .to_string();

        // 按**文件头**判格式，不看扩展名 —— 改过后缀名的图片很常见，
        // 而内容类型与媒体部件名一旦写错，Word 会拒绝打开整份文档。
        let (ext, ct, px_w, px_h) = detect(&bytes).with_context(|| {
            format!(
                "认不出图片格式（{}）—— 目前支持 PNG / JPEG / GIF / BMP",
                path.display()
            )
        })?;
        if px_w == 0 || px_h == 0 {
            bail!("图片宽或高为 0，无法等比缩放：{}", path.display());
        }
        Ok(ImageBytes {
            bytes,
            ext,
            content_type: ct,
            px_w,
            px_h,
            label,
        })
    }
}

/// 认格式 + 取原始像素尺寸。
///
/// 每个分支的长度判断都写成"**够不够读到最后一个字节**"，而不是"比它大"：
/// 一个 24 字节的最小 PNG 正好要读到 `b[23]`，写成 `len > 24` 就会把合法文件判掉
/// （实测踩过：测试里那张 24 字节的 PNG 就栽在这个 `>` 上）。
fn detect(b: &[u8]) -> Result<(&'static str, &'static str, u32, u32)> {
    // PNG：签名 8 字节 + IHDR 长度 4 + 类型 4 + 宽 4 + 高 4 = 24 字节起
    if b.len() >= 24 && b.starts_with(b"\x89PNG\r\n\x1a\n") && &b[12..16] == b"IHDR" {
        let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
        let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
        return Ok(("png", "image/png", w, h));
    }
    // GIF：逻辑屏幕宽高是小端 u16（偏移 6..10）
    if b.len() >= 10 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        let w = u16::from_le_bytes([b[6], b[7]]) as u32;
        let h = u16::from_le_bytes([b[8], b[9]]) as u32;
        return Ok(("gif", "image/gif", w, h));
    }
    // BMP：DIB 头里是**有符号** u32，宽高取绝对值；偏移 18 / 22
    if b.len() >= 26 && b.starts_with(b"BM") {
        let w = i32::from_le_bytes([b[18], b[19], b[20], b[21]]).unsigned_abs();
        let h = i32::from_le_bytes([b[22], b[23], b[24], b[25]]).unsigned_abs();
        return Ok(("bmp", "image/bmp", w, h));
    }
    // JPEG：逐段找 SOF（0xC0..0xCF，但 C4=DHT / C8=JPG / CC=DAC 不是 SOF）
    if b.len() >= 4 && b[0] == 0xFF && b[1] == 0xD8 {
        let mut i = 2usize;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            if marker == 0xFF {
                i += 1; // 填充字节
                continue;
            }
            // 无长度字段的独立标记：SOI / RSTn / TEM
            if marker == 0xD8 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
                i += 2;
                continue;
            }
            let seg_len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            let is_sof = (0xC0..=0xCF).contains(&marker)
                && marker != 0xC4
                && marker != 0xC8
                && marker != 0xCC;
            if is_sof {
                let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
                let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
                return Ok(("jpeg", "image/jpeg", w, h));
            }
            if seg_len < 2 {
                break; // 长度字段坏了，别再往前滚（防死循环）
            }
            i += 2 + seg_len;
        }
        bail!("JPEG 里找不到 SOF 段（文件可能被截断）");
    }
    bail!("既不是 PNG，也不是 JPEG / GIF / BMP")
}

/// 挑一个包里没有的媒体部件名。
fn alloc_media_name(existing: &[String], ext: &str) -> String {
    for n in 1..1000u32 {
        let cand = format!("word/media/wrepl-hdr-{n}.{ext}");
        if !existing.iter().any(|e| e == &cand) {
            return cand;
        }
    }
    // 1000 个都占着：几乎不可能。真碰上就用条目总数兜底（不追求可复现，只为不撞名）
    format!("word/media/wrepl-hdr-{}.{ext}", existing.len())
}

// ─────────────────────────── 页眉定位 ───────────────────────────

/// `w:headerReference/@w:type` 的三个合法取值，按固定序号排列。
///
/// 顺序是**有含义的**：`[i]` 既是"第几号抽屉"，也是日志里给人看的名字。
/// 三个抽屉各自独立继承（见 [`section_headers`]），所以必须分开存。
const HDR_KINDS: [&str; 3] = ["default", "first", "even"];

/// 中文名，只用于日志。
fn kind_cn(k: &str) -> &'static str {
    match k {
        "default" => "主页眉",
        "first" => "首页眉",
        "even" => "偶页眉",
        _ => "页眉",
    }
}

/// 一个页眉部件，以及它被哪些类型引用过。
#[derive(Debug, PartialEq, Eq)]
struct HeaderRef {
    /// 包内部件名（`word/header2.xml`）。
    part: String,
    /// 引用类型（`HDR_KINDS` 的子集，按 `HDR_KINDS` 顺序、不重复）。
    kinds: Vec<&'static str>,
}

/// 一份文档里的页眉引用情况。
#[derive(Debug, Default, PartialEq, Eq)]
struct SectionHeaders {
    /// **要改的页眉部件**：凡是被任何节引用到的都在这里（去重，按首次被引用的顺序）。
    /// 三种 `w:type` 全算 —— `default` / `first` / `even`。
    refs: Vec<HeaderRef>,
}

impl SectionHeaders {
    fn parts(&self) -> Vec<String> {
        self.refs.iter().map(|r| r.part.clone()).collect()
    }

    /// 给人看的一行摘要，例如 `word/header2.xml（主页眉、首页眉）、word/header3.xml（偶页眉）`。
    fn describe(&self) -> String {
        self.refs
            .iter()
            .map(|r| {
                let ks: Vec<&str> = r.kinds.iter().map(|k| kind_cn(k)).collect();
                format!("{}（{}）", r.part, ks.join("、"))
            })
            .collect::<Vec<_>>()
            .join("、")
    }
}

/// 用 `document.xml` + 它的关系表，算出**所有被引用到的页眉部件**（去重、保序）。
///
/// 三条容易漏的规则：
///
/// * **三种类型各自独立继承**：某节没写 `w:type="first"` 的引用时，它的首页眉沿用
///   **前一节**的首页眉；`default` / `even` 各管各、互不干扰。只有第一节没引用
///   才是真的没有。漏了这条，多节文档里就会少改几处 —— 而"少改"最难发现。
/// * **去重按部件、不按节**：几个节共用同一个页眉部件是常态（同一份模板里
///   `header2.xml` 被三个节引用很常见）。不去重就会对同一个部件改两遍 ——
///   第二遍在已改过的内容上再定位再插图，结果是白做功。
/// * **同一部件可身兼数职**：`header2.xml` 完全可能同时是某节的 `default` 和另一节的
///   `even`。部件只改一次，但两个身份都要记住（日志里要说清楚），所以
///   [`HeaderRef::kinds`] 是个列表。
///
/// 另外**不看 `w:titlePg` / `evenAndOddHeaders`**：那两个开关只决定"当前显示不显示"
/// （分别在 `sectPr` 和 `settings.xml` 里），不决定"内容该不该是新的"。一律改掉。
fn section_headers(doc_xml: &[u8], rels: &HashMap<String, String>) -> SectionHeaders {
    let mut out = SectionHeaders::default();
    // 上一节各类型的有效页眉。`None` = 到上一节为止还没有这个类型的页眉。
    let mut last: [Option<String>; 3] = [None, None, None];

    // 按字节顺序扫过所有 `<w:sectPr>` —— 这就是文档里节的先后顺序：
    // 段落里的 sectPr 表示"这一节到此为止"，正文末尾那个是最后一节。
    let mut from = 0usize;
    while let Some(sec) = find_open(doc_xml, from, "sectPr") {
        let (content_from, content_to) = if sec.self_closing {
            (sec.open_end, sec.open_end)
        } else {
            match find_close(doc_xml, sec.open_end, "sectPr") {
                Some(c) => (sec.open_end, c.start),
                None => {
                    from = sec.open_end;
                    continue;
                }
            }
        };
        from = content_to.max(sec.open_end);

        // 本节自己声明的引用，逐个落进对应的抽屉。`type` 缺省即 default。
        let mut p = content_from;
        while let Some(h) = find_open(doc_xml, p, "headerReference") {
            if h.start >= content_to {
                break;
            }
            let tag = &doc_xml[h.start..h.open_end];
            let ty = attr_of(tag, ":type").unwrap_or_else(|| "default".to_string());
            // 非法 type 直接忽略（ECMA-376 只允许 default/first/even）
            if let Some(i) = HDR_KINDS.iter().position(|k| *k == ty) {
                if let Some(part) = attr_of(tag, ":id")
                    .as_deref()
                    .and_then(|rid| rels.get(rid))
                    .cloned()
                {
                    last[i] = Some(part);
                }
            }
            p = h.open_end;
        }

        // 本节三种类型的**有效**页眉 = 本节声明的 + 从上一节继承来的
        for (i, slot) in last.iter().enumerate() {
            let Some(part) = slot else { continue };
            match out.refs.iter_mut().find(|r| &r.part == part) {
                Some(r) => {
                    if !r.kinds.contains(&HDR_KINDS[i]) {
                        r.kinds.push(HDR_KINDS[i]);
                    }
                }
                None => out.refs.push(HeaderRef {
                    part: part.clone(),
                    kinds: vec![HDR_KINDS[i]],
                }),
            }
        }
    }
    out
}

/// 解析一份 `.rels`，返回 `rId → 包内部件路径`（相对路径已补成 `word/xxx` 形状）。
fn parse_rels(rels: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut from = 0usize;
    while let Some(r) = find_open(rels, from, "Relationship") {
        let tag = &rels[r.start..r.open_end];
        if let (Some(id), Some(target)) = (attr_of(tag, ":Id"), attr_of(tag, ":Target")) {
            out.insert(id, resolve_target(&target));
        }
        from = r.open_end;
    }
    out
}

/// 关系里的 `Target` 是**相对于所在部件目录**的，也可能写成绝对路径（以 `/` 开头）。
/// 页眉引用全部落在 `word/` 下，所以这里只需处理这两种。
fn resolve_target(target: &str) -> String {
    let t = target.replace('\\', "/");
    if let Some(rest) = t.strip_prefix('/') {
        return rest.to_string();
    }
    format!("word/{t}")
}

/// 页眉部件名 → 它自己的关系表名。
///
/// 关系表一律是「同目录 + `_rels/` + 原名 + `.rels`」：
/// `word/header2.xml` → `word/_rels/header2.xml.rels`。
fn part_rels_name(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, base)) => format!("{dir}/_rels/{base}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// `word/header2.xml` 这种**页眉**部件名。
///
/// 只管页眉，**不含页脚**。第一版把 `footer*.xml` 也算进来了，于是在真机上每份
/// 文档都多出一句"包内另有 word/footer1.xml、word/footer2.xml、word/footer3.xml
/// 个页眉部件不被任何节引用（未处理）" —— 两处都错：页脚当然被节引用（走
/// `w:footerReference`，我根本没去解析），而且本工具**从设计上就不碰页脚**。
/// 把不在范围内的东西说成"没处理"，是在制造用户看不懂的疑问。
/// 页脚不归本工具管这件事，属于界面文案的职责（见 `wordtool` 的提示文字）。
fn is_header_part(name: &str) -> bool {
    let Some(base) = name.strip_prefix("word/") else {
        return false;
    };
    base.starts_with("header") && base.ends_with(".xml") && !base.contains('/')
}

/// 页眉里「第一个表格的第一个单元格」的坐标（都是该部件 XML 内的字节偏移）。
struct Cell {
    /// `<w:tc …>` 开标签结束位置（内容起点）
    open_end: usize,
    /// `</w:tc>` 起始位置（内容终点）
    close_start: usize,
    /// `<w:tbl …>` 开标签结束位置 —— 没有 `w:tblPr` 时要往这里插一个
    tbl_open_end: usize,
    /// `<w:tr …>` 开标签结束位置 —— 没有 `w:trPr` 时要往这里插一个
    tr_open_end: usize,
    /// 格子自带的属性块 `w:tcPr`
    tcpr: Option<Block>,
    /// 所在表格的 `w:tblPr`
    tblpr: Option<Block>,
    /// 所在行的 `w:trPr`
    trpr: Option<Block>,
    /// 这一行是否**自动行高**（没写死高度）
    row_auto: bool,
    /// 表格首列宽（twip）：`w:gridCol` 优先，退回单元格的 `w:tcW`
    col_w: Option<u32>,
    /// 首行高（twip）：`w:trHeight` 里写死的那个（自动行高时为 None）
    row_h: Option<u32>,
}

/// 定位「第一个表格 → 第一行 → 第一个单元格」。
///
/// ⚠️ 这里**只按局部名找、不看命名空间**，与 `docx::scan` 的口径不同。理由：
/// `tbl` / `tr` / `tc` 这三个局部名在 OOXML 里没有被别的命名空间占用
/// （`scan` 之所以必须判命名空间，是因为 DrawingML 里也有 `a:p` / `a:r` / `a:t`）。
/// 但**嵌套必须处理**：表格就住在单元格里，`<w:tbl>` 会套 `<w:tbl>`。
/// 好在顺序上"第一个 `w:tbl` 里的第一个 `w:tr`"一定是外层行 ——
/// 内层表格的行必然排在某个外层单元格之后，不可能更靠前；`w:tc` 同理。
fn locate_first_cell(xml: &[u8]) -> Option<Cell> {
    let tbl = find_open(xml, 0, "tbl")?;
    if tbl.self_closing {
        return None;
    }
    let tbl_end = find_close(xml, tbl.open_end, "tbl")?.start;

    let tr = find_open(xml, tbl.open_end, "tr")?;
    if tr.self_closing || tr.start > tbl_end {
        return None;
    }
    let tr_end = find_close(xml, tr.open_end, "tr")?.start;

    let tc = find_open(xml, tr.open_end, "tc")?;
    if tc.self_closing || tc.start > tr_end {
        return None;
    }
    let close_start = find_close(xml, tc.open_end, "tc")?.start;

    // `w:tcPr` / `w:tblPr` / `w:trPr` 在 schema 里都必须排在**最前**，
    // 所以只认"紧跟在开标签之后的那个"（见 first_child_block 的说明）。
    let tcpr = first_child_block(xml, tc.open_end, "tcPr");
    let tblpr = first_child_block(xml, tbl.open_end, "tblPr");
    let trpr = first_child_block(xml, tr.open_end, "trPr");

    // 首列宽：表格网格里的第一条 `gridCol` 最权威（Word 写 `tcW` 时就是抄它）；
    // 没写 gridCol 才退回单元格自己的 `tcW`。
    let col_w = tblpr
        .and_then(|_| find_open(xml, tbl.open_end, "gridCol"))
        .filter(|g| g.start < tbl_end)
        .and_then(|g| attr_num(&xml[g.start..g.open_end], ":w"))
        .or_else(|| {
            let b = tcpr?;
            let inner = &xml[b.content_from..b.content_to];
            let g = find_open(inner, 0, "tcW")?;
            let tag = &inner[g.start..g.open_end];
            // `w:type="auto"` 的 tcW 只是占位，不是宽度
            if attr_of(tag, ":type").is_some_and(|t| t == "auto") {
                return None;
            }
            attr_num(tag, ":w")
        });

    // 首行高：`w:trHeight` 带 `w:val` 且 hRule 不是 auto 才算"写死了"。
    // hRule 缺省即 auto —— 那种行的真实高度由内容决定，取不到。
    let (row_h, row_auto) = match trpr {
        Some(b) => {
            let inner = &xml[b.content_from..b.content_to];
            match find_open(inner, 0, "trHeight") {
                Some(g) => {
                    let tag = &inner[g.start..g.open_end];
                    let rule = attr_of(tag, ":hRule").unwrap_or_else(|| "auto".to_string());
                    let val = attr_num(tag, ":val");
                    if rule == "auto" || val.is_none() {
                        (None, true)
                    } else {
                        (val, false)
                    }
                }
                None => (None, true),
            }
        }
        None => (None, true),
    };

    Some(Cell {
        open_end: tc.open_end,
        close_start,
        tbl_open_end: tbl.open_end,
        tr_open_end: tr.open_end,
        tcpr,
        tblpr,
        trpr,
        row_auto,
        col_w,
        row_h,
    })
}

/// 元素 `name` 是否是 `at` 处的**第一个子元素块**；是则返回它的完整坐标。
///
/// 为什么要这一条：`w:tcPr` / `w:tblPr` / `w:trPr` 在 schema 里都必须排在**最前**。
/// 若某处的 `w:tcPr` 不是第一个子元素（手工改过的 XML 就可能这样），那它未必是
/// 这个单元格的属性块，照它下手就是错的 —— 宁可当成"没有属性块"。
fn first_child_block(xml: &[u8], at: usize, name: &str) -> Option<Block> {
    let mut p = at;
    while p < xml.len() && (xml[p] as char).is_ascii_whitespace() {
        p += 1;
    }
    let tag = find_open(xml, p, name)?;
    if tag.start != p {
        return None;
    }
    if tag.self_closing {
        return Some(Block {
            start: tag.start,
            content_from: tag.open_end,
            content_to: tag.open_end,
            end: tag.open_end,
        });
    }
    let close = find_close(xml, tag.open_end, name)?;
    Some(Block {
        start: tag.start,
        content_from: tag.open_end,
        content_to: close.start,
        end: close.end,
    })
}

// ─────────────────────────── 极小 XML 扫描器 ───────────────────────────
//
// 只要三件事：找元素的**开标签**、找它对应的**闭标签**、取属性值。
// 与 `docx::scan` 同一个路子 —— 流式扫描记偏移，不解析成树再序列化回去，
// 否则属性顺序、命名空间前缀、缩进都会被改写。

/// 一个开标签的坐标。
struct OpenTag {
    /// `<` 的位置
    start: usize,
    /// `>` 之后的位置
    open_end: usize,
    /// `<w:x/>` 这种自闭合
    self_closing: bool,
}

/// 一个闭标签的坐标。
struct CloseTag {
    /// `<` 的位置
    start: usize,
    /// `>` 之后的位置
    end: usize,
}

/// 一个元素块的完整坐标。**所有偏移都靠扫描得出**，没有任何"标签长度 = 常量 + 名字长度"
/// 的算式 —— XML 前缀长度不受约束，猜长度就是坏文档的开始。
#[derive(Clone, Copy)]
struct Block {
    /// `<tag …>` 的 `<`
    start: usize,
    /// `>` 之后（内容起点）
    content_from: usize,
    /// `</tag>` 的 `<`（内容终点）
    content_to: usize,
    /// `</tag>` 的 `>` 之后
    end: usize,
}

/// 从 `from` 起找局部名为 `name` 的**开标签**（自动跳过闭标签）。
///
/// ⚠️ 这里必须同时认**两种写法**：
///
/// * 带前缀 —— `w:tbl`（`document.xml` / 页眉里到处是）；
/// * 不带前缀 —— `Relationship` / `Default` / `Types`（`.rels` 与
///   `[Content_Types].xml` 里的元素名就是这么写的）。
///
/// 第一版只拼 `":{name}"` 去找，等于只认前者；后者一律找不到。
/// 后果不是"少改一处"，而是 `parse_rels` 返回空表 → 节里所有页眉引用都解析不出来
/// → 报"这一份文档没有任何页眉引用"。**功能静默失效，理由还是假的。**
fn find_open(xml: &[u8], from: usize, name: &str) -> Option<OpenTag> {
    let nb = name.as_bytes();
    let mut i = from;
    while i + nb.len() <= xml.len() {
        let p = find_sub(&xml[i..], nb)?;
        let at = i + p; // 名字首字符
        let after = at + nb.len();

        // 右边：`w:tbl` 不能命中 `w:tblGrid` / `w:tblPr`
        let right_ok = matches!(
            xml.get(after).copied(),
            Some(b'>') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'/')
        );
        // 左边：要么紧跟 `<`（无前缀），要么是 `前缀:`（有前缀）
        let left_ok = at > 0 && (xml[at - 1] == b'<' || xml[at - 1] == b':');

        if right_ok && left_ok {
            let start = if xml[at - 1] == b'<' {
                at - 1
            } else {
                // 有前缀：往回走到 `<`。中间不许出现 `>` 或空白 ——
                // 否则说明这个 `name` 是落在属性值/正文里的，不是元素名。
                let Some(lt) = xml[..at].iter().rposition(|&c| c == b'<') else {
                    i = at + 1;
                    continue;
                };
                if xml[lt..at].contains(&b'>')
                    || xml[lt..at].iter().any(|c| c.is_ascii_whitespace())
                {
                    i = at + 1;
                    continue;
                }
                lt
            };
            if xml.get(start + 1) == Some(&b'/') {
                i = at + 1; // 是闭标签
                continue;
            }
            let gt = xml[after..].iter().position(|&c| c == b'>')?;
            let open_end = after + gt + 1;
            return Some(OpenTag {
                start,
                open_end,
                self_closing: xml[open_end - 2] == b'/',
            });
        }
        i = at + 1;
    }
    None
}

/// 一个标签的骨架：是开还是闭、局部名是什么、到哪结束。
struct TagSkeleton {
    is_close: bool,
    /// 局部名（最后一个 `:` 之后那段）—— `w:tbl` → `tbl`，`Relationship` → `Relationship`
    local: String,
    /// `>` 之后
    end: usize,
    self_closing: bool,
}

/// 解出 `at`（指向 `<`）处的那个标签。解不出返回 `None`。
fn tag_at(xml: &[u8], at: usize) -> Option<TagSkeleton> {
    let mut i = at + 1;
    let is_close = xml.get(i) == Some(&b'/');
    if is_close {
        i += 1;
    }
    let ns = i;
    while i < xml.len() && xml[i] != b'>' && xml[i] != b'/' && !xml[i].is_ascii_whitespace() {
        i += 1;
    }
    if i == ns {
        return None; // 名字是空的（`<>` / `</>`，非法）
    }
    let full = String::from_utf8_lossy(&xml[ns..i]);
    let local = full.rsplit(':').next().unwrap_or(&full).to_string();
    let gt = xml[i..].iter().position(|&c| c == b'>')? + i;
    Some(TagSkeleton {
        is_close,
        local,
        end: gt + 1,
        self_closing: gt > 0 && xml[gt - 1] == b'/',
    })
}

/// 找 `from`（某开标签之后）对应的闭标签，**按深度计数** ——
/// 嵌套同名元素（表格套表格）不会提前收尾。
///
/// 与 [`find_open`] 同样认两种写法：统一解出"局部名"再比，前缀问题就没了。
fn find_close(xml: &[u8], from: usize, name: &str) -> Option<CloseTag> {
    let mut depth = 1usize;
    let mut i = from;
    while i < xml.len() {
        let lt = xml[i..].iter().position(|&c| c == b'<')? + i;
        // 解不出骨架（注释、`<?xml`、正文里的裸 `<`）就跳过这一个 `<`，别整段放弃
        let Some(t) = tag_at(xml, lt) else {
            i = lt + 1;
            continue;
        };
        if t.local == name {
            if t.is_close {
                depth -= 1;
                if depth == 0 {
                    return Some(CloseTag {
                        start: lt,
                        end: t.end,
                    });
                }
            } else if !t.self_closing {
                depth += 1;
            }
        }
        i = t.end;
    }
    None
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// 逐个产出开标签里的 `(属性名, 属性值)`。
///
/// 必须真的按"属性名 = 值"的语法走一遍，**不能用 `find(":type")` 那种子串搜索**：
/// 属性值里完全可能含 `:type`（`w:val="a:type=b"`），一搜就错位。
/// 全程在字节层比对，只把结果转成 `String` —— 元素名/属性名里可能有非 ASCII
/// （`<x专:tbl …>`），按 `&str` 切字节会切在字符中间而 panic。
fn each_attr(tag: &[u8]) -> Vec<(String, String)> {
    let (mut out, mut i) = (Vec::new(), 0usize);
    if tag.first() == Some(&b'<') {
        i = 1;
    }
    // 跳过元素名（含前缀），走到第一个空白
    while i < tag.len() && !tag[i].is_ascii_whitespace() && tag[i] != b'/' && tag[i] != b'>' {
        i += 1;
    }
    while i < tag.len() {
        while i < tag.len() && tag[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag.len() || tag[i] == b'/' || tag[i] == b'>' {
            break;
        }
        let ns = i;
        while i < tag.len() && tag[i] != b'=' && !tag[i].is_ascii_whitespace() {
            i += 1;
        }
        let name = String::from_utf8_lossy(&tag[ns..i]).into_owned();
        while i < tag.len() && tag[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag.len() || tag[i] != b'=' {
            break; // XML 不允许无值属性；真遇上就收手，别死循环
        }
        i += 1;
        while i < tag.len() && tag[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag.len() || (tag[i] != b'"' && tag[i] != b'\'') {
            break;
        }
        let q = tag[i];
        i += 1;
        let vs = i;
        while i < tag.len() && tag[i] != q {
            i += 1;
        }
        out.push((name, String::from_utf8_lossy(&tag[vs..i]).into_owned()));
        i += 1; // 跳过收尾引号
    }
    out
}

/// 取属性值。`key` 以 `:` 开头表示**按局部名匹配**（忽略命名空间前缀），
/// 否则按整名匹配。
///
/// 为什么要按局部名：同一份 OOXML 里属性的前缀写法并不统一 ——
/// `document.xml` 里是 `w:type` / `r:id`，而 `.rels` 里是**没有前缀的**
/// `Id` / `Type` / `Target`，`[Content_Types].xml` 里是 `Extension` / `ContentType`。
/// 只认 `w:type` 那种写法，会让"已有关系就复用"、"已有扩展名就不插"两条判断全部失手，
/// 后果是关系表越堆越长、内容类型表出现重复条目。
fn attr_of(tag: &[u8], key: &str) -> Option<String> {
    let loose = key.starts_with(':');
    let want = key.trim_start_matches(':');
    each_attr(tag)
        .into_iter()
        .find(|(name, _)| {
            if loose {
                // 局部名 = 最后一个 ':' 之后那一段
                name.rsplit(':').next() == Some(want)
            } else {
                name == want
            }
        })
        .map(|(_, v)| v)
}

/// 属性值当无符号整数取；不是纯数字就 `None`。
fn attr_num(tag: &[u8], key: &str) -> Option<u32> {
    attr_of(tag, key)?.trim().parse::<u32>().ok()
}

// ─────────────────────────── XML 生成 ───────────────────────────

/// Word 手动换行符（`Chr(11)`）在 OOXML 里就是 `<w:br/>`。
const BR: &str = "<w:br/>";
const EAST_ASIA_FONT: &str = "宋体";
const LATIN_FONT: &str = "Times New Roman";
/// 9 磅 = 小五；OOXML 的 `w:sz` 单位是**半磅**。
const HALF_POINTS: u32 = 18;

/// 文本模式：该单元格的完整新内容（含 `w:tcPr` 原样抄回）。
///
/// 与 COM 版（`services::replace_in_cell`）的对应关系：
///
/// | COM | 这里 |
/// |---|---|
/// | `cell.Range.Text = ""` | 整段换掉（旧文本、旧图片一起没了） |
/// | `= "EN" & Chr(11) & "CN"` | 一个 run：`<w:t>EN</w:t><w:br/><w:t>CN</w:t>` |
/// | `Font.NameFarEast = 宋体` | `w:rFonts w:eastAsia="宋体"` |
/// | `Font.Name = Times New Roman` | `w:rFonts w:ascii/hAnsi/cs="Times New Roman"` |
/// | `Font.Size = 9` | `w:sz w:val="18"` |
/// | `Bold/Italic = 0`、`Underline = none` | `w:b/@w:val="0"`、`w:i/@w:val="0"`、`w:u/@w:val="none"` |
///
/// 段落属性（`w:pPr`）**原样保留** —— COM 那边也没动它（`Range.Text = ""` 不会
/// 换掉段落标记的格式）。
fn text_cell_xml(hdr: &[u8], cell: &Cell, en: &str, cn: &str) -> String {
    let mut s = String::with_capacity(512);
    if let Some(b) = cell.tcpr {
        s.push_str(&String::from_utf8_lossy(&hdr[b.start..b.end]));
    }
    s.push_str("<w:p>");
    s.push_str(&paragraph_lead(hdr, cell));
    s.push_str(&text_run_xml(en, cn));
    s.push_str("</w:p>");
    s
}

/// 取单元格里第一个段落的 `w:pPr` 原文（若有），拼在 `inner` 前面。
///
/// 只有**紧跟在 `<w:p …>` 之后的**那个 `pPr` 才算数（schema 要求它排最前）。
fn paragraph_lead(hdr: &[u8], cell: &Cell) -> String {
    let Some(p) = find_open(&hdr[cell.open_end..cell.close_start], 0, "p") else {
        return String::new();
    };
    let base = cell.open_end + p.open_end;
    match first_child_block(hdr, base, "pPr") {
        Some(b) => String::from_utf8_lossy(&hdr[b.start..b.end]).into_owned(),
        None => String::new(),
    }
}

/// 「英文 + 手动换行 + 中文」；只有一种语言时就只有它（同 COM 的 `compose`）。
fn text_run_xml(en: &str, cn: &str) -> String {
    let (en, cn) = (en.trim(), cn.trim());
    let mut body = String::with_capacity(320);
    // ⚠️ `w:rPr` 的子元素顺序是 schema 里的 **sequence**：
    //    rFonts → b/bCs → i/iCs → … → sz/szCs → … → u
    //    写反了 Word 会报"文件已损坏"。
    body.push_str("<w:rPr>");
    body.push_str(&format!(
        "<w:rFonts w:ascii=\"{LATIN_FONT}\" w:hAnsi=\"{LATIN_FONT}\" \
         w:eastAsia=\"{EAST_ASIA_FONT}\" w:cs=\"{LATIN_FONT}\"/>"
    ));
    body.push_str(r#"<w:b w:val="0"/><w:i w:val="0"/>"#);
    body.push_str(&format!(
        r#"<w:sz w:val="{HALF_POINTS}"/><w:szCs w:val="{HALF_POINTS}"/>"#
    ));
    body.push_str(r#"<w:u w:val="none"/>"#);
    body.push_str("</w:rPr>");

    body.push_str(r#"<w:t xml:space="preserve">"#);
    body.push_str(&esc(en));
    body.push_str("</w:t>");
    if !en.is_empty() && !cn.is_empty() {
        body.push_str(BR);
    }
    if !cn.is_empty() {
        body.push_str(r#"<w:t xml:space="preserve">"#);
        body.push_str(&esc(cn));
        body.push_str("</w:t>");
    }
    format!("<w:r>{body}</w:r>")
}

/// 图片模式：单元格新内容 + 表格属性上的侧路编辑。
///
/// 返回 `(单元格内容 XML, 表格属性编辑列表)`。分两路是因为**锁列与行高要改的地方
/// 在单元格之外**（`w:tblPr` / `w:trPr`），与单元格那段不重叠，可以一起落刀。
///
/// 与 COM 版的对应：`center_in_cell` 的 `Alignment=center` / `SpaceBefore=SpaceAfter=0`
/// → 段落 `w:jc=center` + `w:spacing before/after=0`；`cell.VerticalAlignment = center`
/// → `w:tcPr/w:vAlign=center`；`tbl.AllowAutoFit=false` + `col(1).PreferredWidth`
/// → `w:tblLayout=fixed` + 首列 `w:tcW`；`fix_row_height_if_auto` → `w:trHeight`。
fn image_cell_edits(
    hdr: &[u8],
    cell: &Cell,
    img: &ImageBytes,
    scale: bool,
    rid: &str,
) -> Result<(String, Vec<Ed>)> {
    let (cx, cy) = picture_extent(cell, img, scale)?;

    let mut s = String::with_capacity(1400);
    s.push_str(&patched_tcpr(hdr, cell, scale));
    s.push_str("<w:p>");
    s.push_str(&image_ppr(hdr, cell));
    s.push_str("<w:r>");
    s.push_str(&drawing_xml(cx, cy, rid, &img.label));
    s.push_str("</w:r>");
    s.push_str("</w:p>");

    let mut side: Vec<Ed> = Vec::new();
    if scale {
        side.push(cell_width_lock(hdr, cell));
        if cell.row_auto {
            // 自动行高的行：图片撑不开它，会把图截掉 —— 按图片高度 +2 磅给个下限。
            //
            // 用 `atLeast`（"至少"）而不是 `exact`（"恰好"）：exact 一旦算小一点
            // 就是硬裁；atLeast 的语义是"别比这个矮"，多了算赚。
            // ⚠️ 顺带记一笔：被本次改造替换掉的 COM 版这里是 `HeightRule = 1`，
            //    而 Word 的枚举是 `wdRowHeightAtLeast = 1` / `wdRowHeightExactly = 2`
            //    —— 也就是说那份代码想要"恰好"，实际给的是"至少"。
            //    这里**保持实际行为**（至少），并把它写对。
            let need = cy / EMU_PER_TWIP + 40; // 图片高 + 2 磅(40 twip)
            side.push(upsert_child(
                hdr,
                cell.trpr,
                cell.tr_open_end,
                "trPr",
                "trHeight",
                &format!(r#"w:val="{need}" w:hRule="atLeast""#),
            ));
        }
    }
    Ok((s, side))
}

/// 锁列：表格改为固定布局，并把首列宽度写进单元格的 `w:tcW`。
///
/// 为什么"锁"就是写 `w:tblLayout=fixed`：Word 的默认布局是自动调整，
/// 一张比格子大的图会把整张表撑开（页眉的版式就散了）。固定布局下表格只认
/// `gridCol` / `tcW` 的宽度，图片再大也只是被"装"在格子里 —— 这正是 COM 版
/// `AllowAutoFit = false` 想要的。宽度值取**表格自身的 gridCol**，所以这一步
/// 不会改变表格原本的版式，只是把它"钉住"。
fn cell_width_lock(hdr: &[u8], cell: &Cell) -> Ed {
    upsert_child(
        hdr,
        cell.tblpr,
        cell.tbl_open_end,
        "tblPr",
        "tblLayout",
        r#"w:type="fixed""#,
    )
}

/// 在 `block` 内部把子元素 `name` 换成 `<w:name attrs/>`；没有就追加进去。
///
/// `block` 为 `None`（这个元素压根没有属性块）时，把**整个属性块**补在 `anchor`
/// ——也就是父元素开标签之后。`w:tblPr` / `w:trPr` 在 schema 里都必须是父元素的
/// **第一个**子元素，插在开标签之后正好。
///
/// 只处理自闭合形态（`<w:tcW …/>`、`<w:trHeight …/>`、`<w:tblLayout …/>` 都是这种，
/// Word 从不写成带子元素的形态）。遇到非自闭合的同名元素就整块换掉，
/// 避免出现两个同名子元素（那是 schema 违规）。
fn upsert_child(
    hdr: &[u8],
    block: Option<Block>,
    anchor: usize,
    block_name: &str,
    child_name: &str,
    attrs: &str,
) -> Ed {
    match block {
        Some(b) => {
            let inner = &hdr[b.content_from..b.content_to];
            let new_inner =
                upsert_self_closing(&String::from_utf8_lossy(inner), child_name, attrs);
            Ed {
                at: b.content_from,
                del: b.content_to - b.content_from,
                ins: new_inner.into_bytes(),
            }
        }
        None => Ed {
            at: anchor,
            del: 0,
            ins: format!("<w:{block_name}>{}</w:{block_name}>", closed(child_name, attrs))
                .into_bytes(),
        },
    }
}

/// 拼一个自闭合子元素。`attrs` **只放属性串** —— 不带元素名、也不带结尾的 `/>`，
/// 例如 `w:val="center"`。
///
/// 这条约定必须统一：调用点一旦有谁把 `/>` 也塞进来，就会拼出
/// `<w:vAlign <w:val="center"/>` 这种**截断的标签**，Word 打开直接报"文件已损坏"。
/// 所以由本函数独家收尾，调用方没有机会漏。
fn closed(name: &str, attrs: &str) -> String {
    if attrs.is_empty() {
        format!("<w:{name}/>")
    } else {
        format!("<w:{name} {attrs}/>")
    }
}

/// 在 `block` 里把名为 `name` 的自闭合元素换成 `<w:name attrs/>`；没有就追加。
fn upsert_self_closing(block: &str, name: &str, attrs: &str) -> String {
    let bytes = block.as_bytes();
    let repl = closed(name, attrs);
    if let Some(tag) = find_open(bytes, 0, name) {
        let end = if tag.self_closing {
            tag.open_end
        } else {
            match find_close(bytes, tag.open_end, name) {
                Some(c) => c.end,
                None => tag.open_end,
            }
        };
        let mut s = String::with_capacity(block.len() + repl.len());
        s.push_str(&block[..tag.start]);
        s.push_str(&repl);
        s.push_str(&block[end..]);
        s
    } else {
        format!("{block}{repl}")
    }
}

/// 重写单元格的 `w:tcPr`：补 `vAlign=center`，勾了缩放时再钉死首列宽度。
///
/// `vAlign` 在 schema 的 sequence 里排在 `tcW` / `gridSpan` 之后、`hideMark` 之前；
/// 「有就替掉、没有就追加到末尾」这种朴素做法在**有 `hideMark` 时会把顺序写反**，
/// 于是这里显式挑位置：追加时插到 `hideMark` 之前（没有 hideMark 就追加到末尾）。
fn patched_tcpr(hdr: &[u8], cell: &Cell, scale: bool) -> String {
    let inner = match cell.tcpr {
        Some(b) => String::from_utf8_lossy(&hdr[b.content_from..b.content_to]).into_owned(),
        None => String::new(),
    };
    let mut inner = upsert_before_marker(&inner, "vAlign", r#"w:val="center""#);
    if scale {
        if let Some(w) = cell.col_w {
            inner = upsert_self_closing(&inner, "tcW", &format!(r#"w:w="{w}" w:type="dxa""#));
        }
    }
    format!("<w:tcPr>{inner}</w:tcPr>")
}

/// 与 [`upsert_self_closing`] 相同，但"追加"时插到 `<w:hideMark/>` 之前 ——
/// `vAlign` 在 `w:tcPr` 的 sequence 里排在 `hideMark` 前面。
fn upsert_before_marker(block: &str, name: &str, attrs: &str) -> String {
    if find_open(block.as_bytes(), 0, name).is_some() {
        return upsert_self_closing(block, name, attrs);
    }
    let repl = closed(name, attrs);
    match block.find("<w:hideMark") {
        Some(i) => format!("{}{repl}{}", &block[..i], &block[i..]),
        None => format!("{block}{repl}"),
    }
}

/// 图片段落属性：`w:spacing before/after=0` + `w:jc=center`，保留原段落的
/// `w:pStyle` 与 `w:rPr`。
///
/// 这里**不逐字保留原 `w:pPr`**（文本模式才那么做）：图片模式整段内容都是重写的，
/// 一个 `pStyle` 之外的东西（缩进、行距、制表位……）留着只会让图片位置变得不可预测。
/// 反正 schema 的顺序要求摆在那儿，与其在任意原 pPr 里做插入排序，不如按 canonical
/// 顺序重新拼一个 —— 拼出来的必然合法。
fn image_ppr(hdr: &[u8], cell: &Cell) -> String {
    let (mut pstyle, mut rpr) = (String::new(), String::new());
    if let Some(p) = find_open(&hdr[cell.open_end..cell.close_start], 0, "p") {
        let base = cell.open_end + p.open_end;
        if let Some(b) = first_child_block(hdr, base, "pPr") {
            let full = &hdr[b.start..b.end];
            pstyle = child_element(full, "pStyle").unwrap_or_default();
            rpr = child_element(full, "rPr").unwrap_or_default();
        }
    }
    format!(
        "<w:pPr>{pstyle}<w:spacing w:before=\"0\" w:after=\"0\"/>\
         <w:jc w:val=\"center\"/>{rpr}</w:pPr>"
    )
}

/// 取 `block` 里名为 `name` 的那个子元素（含首尾标签）的原文。
fn child_element(block: &[u8], name: &str) -> Option<String> {
    let tag = find_open(block, 0, name)?;
    let end = if tag.self_closing {
        tag.open_end
    } else {
        find_close(block, tag.open_end, name)?.end
    };
    Some(String::from_utf8_lossy(&block[tag.start..end]).into_owned())
}

/// 算图片最终尺寸（EMU）。
///
/// 与 COM 版 `scale_to_cell` 一致 —— **contain 适配 + 90% 安全边距**：
/// 先按行高缩放，宽度超限就改按列宽；自动行高（取不到高度）时以**图片原高**兜底，
/// 于是得到"缩到原高的 90%"，正好留出上下余量。
///
/// `scale == false` 时直接给原尺寸 —— 与 COM 版"否则保持原尺寸，表格自适应图片"等价。
fn picture_extent(cell: &Cell, img: &ImageBytes, scale: bool) -> Result<(i64, i64)> {
    let nat_w = img.px_w as i64 * EMU_PER_PX;
    let nat_h = img.px_h as i64 * EMU_PER_PX;
    if !scale {
        return Ok((nat_w, nat_h));
    }
    /// 安全边距系数：只占可用区域的 90%，四边各留 5%（同 COM 的 `MARGIN_RATIO`）。
    const MARGIN: f64 = 0.90;

    let avail_w = cell.col_w.map(|t| t as i64 * EMU_PER_TWIP);
    let avail_h = cell.row_h.map(|t| t as i64 * EMU_PER_TWIP);

    let h_for = avail_h.unwrap_or(nat_h);
    let mut k = h_for as f64 * MARGIN / nat_h as f64;
    if let Some(aw) = avail_w {
        if nat_w as f64 * k > aw as f64 * MARGIN {
            k = aw as f64 * MARGIN / nat_w as f64;
        }
    }
    if !(k > 0.0) {
        k = 1.0; // 防御：算不出比例就按原尺寸，别写出 0 宽的图
    }
    let cx = (nat_w as f64 * k).round() as i64;
    let cy = (nat_h as f64 * k).round() as i64;
    if cx <= 0 || cy <= 0 {
        bail!("按表格尺寸算出的图片尺寸非法（{cx}×{cy} EMU）");
    }
    Ok((cx, cy))
}

/// 生成 `w:drawing`。**命名空间全部挂在自己身上**（见模块头第 3 条）。
fn drawing_xml(cx: i64, cy: i64, rid: &str, label: &str) -> String {
    const WP: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
    const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
    const PIC: &str = "http://schemas.openxmlformats.org/drawingml/2006/picture";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

    let name = esc(label);
    // `wp:docPr/@id` 在同一部件内要唯一。这里用固定值：单元格内容已被整段换掉，
    // 原来那个图形的 id 也随之消失，同一部件里撞不上。
    let id = 1;
    format!(
        r#"<w:drawing><wp:inline xmlns:wp="{WP}" distT="0" distB="0" distL="0" distR="0"><wp:extent cx="{cx}" cy="{cy}"/><wp:effectExtent l="0" t="0" r="0" b="0"/><wp:docPr id="{id}" name="{name}"/><a:graphic xmlns:a="{A}"><a:graphicData uri="{PIC}"><pic:pic xmlns:pic="{PIC}"><pic:nvPicPr><pic:cNvPr id="{id}" name="{name}"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip xmlns:r="{R}" r:embed="{rid}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing>"#
    )
}

/// XML 文本转义。`&` 必须最先换，否则后面换出来的实体会被二次转义。
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ─────────────────────────── 部件级改写 ───────────────────────────

/// 一次定点字节编辑：把 `[at, at + del)` 换成 `ins`。
struct Ed {
    at: usize,
    del: usize,
    ins: Vec<u8>,
}

/// 落刀。**按偏移从大到小**依次替换 —— 从后往前改，前面的偏移才不会被改动过的
/// 长度推移（这是唯一不需要在每次编辑后重算偏移的做法）。
///
/// 会断言各段互不重叠：重叠意味着定位算错了，那种情况下"从后往前"也会给出
/// 一份看似正常、实际错位的文档。
fn apply_edits(xml: &[u8], mut eds: Vec<Ed>) -> Vec<u8> {
    eds.sort_by_key(|e| std::cmp::Reverse(e.at));
    for w in eds.windows(2) {
        debug_assert!(
            w[1].at + w[1].del <= w[0].at,
            "编辑区间重叠：[{}..{}) 与 [{}..{})",
            w[1].at,
            w[1].at + w[1].del,
            w[0].at,
            w[0].at + w[0].del
        );
    }
    let mut out = xml.to_vec();
    for e in eds {
        out.splice(e.at..e.at + e.del, e.ins);
    }
    out
}

/// 给页眉部件补上指向图片的关系；返回 `(rId, 新的关系表字节)`。
///
/// `rels` 为 `None` 表示这个页眉原本没有关系表（`word/_rels/headerN.xml.rels`
/// 不存在），此时现建一份。
///
/// `target_pkg` 是**包内全路径**（`word/media/wrepl-hdr-1.png`）。注意写进 rels 的
/// `Target` 必须**相对于该关系表所在部件**（`word/` 下的部件就写 `media/…`）——
/// 照抄全路径会解析成 `word/word/media/…`，图片指向空、Word 里就是一个红叉。
/// 所以这里比较用全路径、落笔用相对路径，两个都算清楚。
///
/// 已经有一条指向同一目标的 image 关系时**复用它**（返回 `None` 表示无需改动）——
/// 同一页眉被反复替换时不会把关系表越堆越长。
fn ensure_image_rel(rels: Option<&[u8]>, target_pkg: &str) -> (String, Option<Vec<u8>>) {
    const IMAGE_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
    const PKG_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

    // 本模块只会往 `word/` 下的页眉里插图，所以相对目标 = 去掉 `word/`。
    let target_rel = target_pkg.strip_prefix("word/").unwrap_or(target_pkg);

    if let Some(bytes) = rels {
        let map = parse_rels(bytes);
        // 同目标复用
        let mut from = 0usize;
        let mut max_n = 0u32;
        while let Some(r) = find_open(bytes, from, "Relationship") {
            let tag = &bytes[r.start..r.open_end];
            if let (Some(id), Some(t), Some(ty)) = (
                attr_of(tag, ":Id"),
                attr_of(tag, ":Target"),
                attr_of(tag, ":Type"),
            ) {
                if resolve_target(&t) == target_pkg && ty == IMAGE_REL {
                    return (id, None);
                }
                if let Some(n) = id.strip_prefix("rId").and_then(|n| n.parse::<u32>().ok()) {
                    max_n = max_n.max(n);
                }
            }
            from = r.open_end;
        }
        // 新 rId：从现有最大编号往后试，跳过已占用的
        let mut n = max_n + 1;
        let id = loop {
            let cand = format!("rId{n}");
            if !map.contains_key(&cand) {
                break cand;
            }
            n += 1;
        };
        let ins = format!(
            r#"<Relationship Id="{id}" Type="{IMAGE_REL}" Target="{}"/>"#,
            esc(target_rel)
        );
        let s = String::from_utf8_lossy(bytes);
        let i = s.rfind("</Relationships>").unwrap_or(s.len());
        let mut out = String::with_capacity(s.len() + ins.len());
        out.push_str(&s[..i]);
        out.push_str(&ins);
        out.push_str(&s[i..]);
        (id, Some(out.into_bytes()))
    } else {
        let out = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>") + &format!(
            r#"<Relationships xmlns="{PKG_NS}">"#
        ) + &format!(
            r#"<Relationship Id="rId1" Type="{IMAGE_REL}" Target="{}"/>"#,
            esc(target_rel)
        ) + "</Relationships>";
        ("rId1".to_string(), Some(out.into_bytes()))
    }
}

/// 给 `[Content_Types].xml` 补一条扩展名默认映射（已有同扩展名就原样返回）。
///
/// ⚠️ 这里曾经用 `s.find("<Default")` 手动切标签、再拿 `attr_of(…, ":Extension")` 取
/// 扩展名 —— 而 `attr_of` 当时只认**带命名空间前缀**的属性名，而内容类型表里写的是
/// 裸的 `Extension="png"`。于是判断永远失手，每次替换都往表里再插一条重复的
/// `<Default Extension="png" …>`。`attr_of` 改成按局部名匹配后这条自然好了，
/// 但代价是当时的"重复条目"其实是坏包（两处都写死同一个映射，Word 行为未定义）。
fn ensure_default_ext(ct: &[u8], ext: &str, content_type: &str) -> Vec<u8> {
    let s = String::from_utf8_lossy(ct);
    // 扩展名大小写不敏感：已有的 `<Default Extension="PNG" …>` 也算数
    let mut from = 0usize;
    while let Some(p) = s[from..].find("<Default") {
        let at = from + p;
        let end = s[at..].find('>').map(|e| at + e + 1).unwrap_or(s.len());
        if let Some(e) = attr_of(s[at..end].as_bytes(), ":Extension") {
            if e.eq_ignore_ascii_case(ext) {
                return ct.to_vec();
            }
        }
        from = end;
    }
    let ins = format!(r#"<Default Extension="{ext}" ContentType="{content_type}"/>"#);
    // 插在第一个 `<Override` 之前（`<Default>` 与 `<Override>` 混排是合法的，
    // 但习惯上 Default 在前）；没有 Override 就插在 `</Types>` 前。
    let i = s
        .find("<Override")
        .or_else(|| s.rfind("</Types>"))
        .unwrap_or(s.len());
    let mut out = String::with_capacity(s.len() + ins.len());
    out.push_str(&s[..i]);
    out.push_str(&ins);
    out.push_str(&s[i..]);
    out.into_bytes()
}

// ─────────────────────────── 测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const HDR: &str = r#"<?xml version="1.0"?><w:hdr xmlns:w="http://w" xmlns:r="http://r"><w:tbl><w:tblPr><w:tblW w:w="9356" w:type="dxa"/><w:tblLayout w:type="fixed"/></w:tblPr><w:tblGrid><w:gridCol w:w="2577"/><w:gridCol w:w="4650"/></w:tblGrid><w:tr><w:trPr><w:trHeight w:hRule="exact" w:val="851"/></w:trPr><w:tc><w:tcPr><w:tcW w:w="2577" w:type="dxa"/><w:vAlign w:val="top"/><w:hideMark/></w:tcPr><w:p><w:pPr><w:pStyle w:val="T14-logo"/></w:pPr><w:r><w:t>DOFFLON</w:t></w:r></w:p></w:tc><w:tc><w:tcPr/><w:p><w:r><w:t>后一格</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:hdr>"#;

    fn img(w: u32, h: u32) -> ImageBytes {
        ImageBytes {
            bytes: vec![],
            ext: "png",
            content_type: "image/png",
            px_w: w,
            px_h: h,
            label: "t.png".into(),
        }
    }

    #[test]
    fn 找得到首个表格的首个单元格() {
        let c = locate_first_cell(HDR.as_bytes()).expect("应当定位成功");
        let cell = String::from_utf8_lossy(&HDR.as_bytes()[c.open_end..c.close_start]);
        assert!(cell.contains("DOFFLON"), "拿到的应当是第一个格子：{cell}");
        assert!(!cell.contains("后一格"), "不该拿到第二个格子");
        assert!(c.tcpr.is_some());
        assert_eq!(c.col_w, Some(2577));
        assert_eq!(c.row_h, Some(851));
        assert!(!c.row_auto);
    }

    #[test]
    fn 嵌套表格不会提前收尾() {
        let xml = r#"<w:hdr><w:tbl><w:tr><w:tc><w:tcPr/><w:p><w:r><w:t>外</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:tcPr/><w:p><w:r><w:t>内</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:tc></w:tr></w:tbl></w:hdr>"#;
        let c = locate_first_cell(xml.as_bytes()).unwrap();
        let cell = String::from_utf8_lossy(&xml.as_bytes()[c.open_end..c.close_start]).to_string();
        assert!(cell.contains("外"), "第一个格子是外层的");
        assert!(cell.contains("内"), "内层表格也在该格子里，应被一起换掉");
        // 换掉之后整个部件仍应是配平的结构
        let out = apply_edits(
            xml.as_bytes(),
            vec![Ed {
                at: c.open_end,
                del: c.close_start - c.open_end,
                ins: b"<w:p/>".to_vec(),
            }],
        );
        let s = String::from_utf8_lossy(&out);
        // 内层表格住在第一个格子里，清格会把它一起带走 —— 所以整份输出**恰好**是
        // 外层表套一个空段落。逐字比对才算真的证明了"配平"：只看标签计数会漏掉
        // "多出一个游离的 `</w:tc>`"这类坏法。
        assert_eq!(
            s,
            "<w:hdr><w:tbl><w:tr><w:tc><w:p/></w:tc></w:tr></w:tbl></w:hdr>"
        );
    }

    #[test]
    fn 行高为auto时取不到高度() {
        let xml = HDR.replace(r#"<w:trHeight w:hRule="exact" w:val="851"/>"#, "");
        let c = locate_first_cell(xml.as_bytes()).unwrap();
        assert_eq!(c.row_h, None);
        assert!(c.row_auto);
    }

    /// 造一份 `document.xml`：`sects` 里每一项是"这一节的页眉引用"，每项是
    /// `(type, rId)` 的列表（`None` 表示本节一条引用都不写）。
    fn doc_of(sects: &[&[(&str, &str)]]) -> String {
        let mut s = String::from("<w:document><w:body>");
        for (i, refs) in sects.iter().enumerate() {
            let inner: String = refs
                .iter()
                .map(|(ty, id)| format!(r#"<w:headerReference w:type="{ty}" r:id="{id}"/>"#))
                .collect();
            // 最后一个是正文末尾那个 sectPr；其余裹在段落里（Word 就是这个形状）
            if i + 1 == sects.len() {
                s.push_str(&format!("<w:sectPr>{inner}</w:sectPr>"));
            } else {
                s.push_str(&format!("<w:p><w:pPr><w:sectPr>{inner}</w:sectPr></w:pPr></w:p>"));
            }
        }
        s.push_str("</w:body></w:document>");
        s
    }

    fn rels_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(id, part)| (id.to_string(), part.to_string()))
            .collect()
    }

    /// 把 `refs` 压成便于断言的 `部件 → 类型列表`。
    fn as_pairs(sh: &SectionHeaders) -> Vec<(String, Vec<&'static str>)> {
        sh.refs
            .iter()
            .map(|r| (r.part.clone(), r.kinds.clone()))
            .collect()
    }

    #[test]
    fn 多个节的同一页眉只算一次() {
        let doc = doc_of(&[
            &[("default", "rId27")],
            &[("default", "rId27")],
        ]);
        let rels = rels_of(&[("rId27", "word/header2.xml")]);
        assert_eq!(
            as_pairs(&section_headers(doc.as_bytes(), &rels)),
            vec![("word/header2.xml".to_string(), vec!["default"])]
        );
    }

    #[test]
    fn 三种页眉类型全都要改() {
        // 一节里 default / first / even 齐全：三个部件都要进结果，且带上各自的类型。
        // 顺序是"抽屉序"（default → first → even），与它们在 XML 里写的先后无关。
        let doc = doc_of(&[&[
            ("first", "rIdA"),
            ("even", "rIdB"),
            ("default", "rIdC"),
        ]]);
        let rels = rels_of(&[
            ("rIdA", "word/header1.xml"),
            ("rIdB", "word/header2.xml"),
            ("rIdC", "word/header3.xml"),
        ]);
        assert_eq!(
            as_pairs(&section_headers(doc.as_bytes(), &rels)),
            vec![
                ("word/header3.xml".to_string(), vec!["default"]),
                ("word/header1.xml".to_string(), vec!["first"]),
                ("word/header2.xml".to_string(), vec!["even"]),
            ]
        );
    }

    #[test]
    fn 三种类型各自独立继承且互不干扰() {
        // 第 1 节声明 default=header2、first=header1；
        // 第 2 节只声明 even=header3 → default/first 沿用，even 新增
        let doc = doc_of(&[
            &[("default", "r1"), ("first", "r2")],
            &[("even", "r3")],
        ]);
        let rels = rels_of(&[
            ("r1", "word/header2.xml"),
            ("r2", "word/header1.xml"),
            ("r3", "word/header3.xml"),
        ]);
        let sh = section_headers(doc.as_bytes(), &rels);
        // 第 1 节：default + first；第 2 节：even（default/first 继承、不重复出现，
        // 但部件的 kinds 要把"两种身份"都记住）
        assert_eq!(
            as_pairs(&sh),
            vec![
                ("word/header2.xml".to_string(), vec!["default"]),
                ("word/header1.xml".to_string(), vec!["first"]),
                ("word/header3.xml".to_string(), vec!["even"]),
            ]
        );
    }

    #[test]
    fn 同一部件兼多种类型只改一次但两个身份都记住() {
        // header3 在第 1 节是 default、在第 2 节是 even（"共用页眉"的典型写法）
        let doc = doc_of(&[&[("default", "rC")], &[("even", "rC")]]);
        let rels = rels_of(&[("rC", "word/header3.xml")]);
        let sh = section_headers(doc.as_bytes(), &rels);
        assert_eq!(
            as_pairs(&sh),
            vec![("word/header3.xml".to_string(), vec!["default", "even"])],
            "同一个部件只出现一条，但两种身份都要记下来"
        );
        assert!(sh.describe().contains("主页眉") && sh.describe().contains("偶页眉"));
    }

    #[test]
    fn 只有首页眉的文档也要改() {
        let doc = doc_of(&[&[("first", "rA")]]);
        let rels = rels_of(&[("rA", "word/header1.xml")]);
        let sh = section_headers(doc.as_bytes(), &rels);
        assert_eq!(sh.parts(), vec!["word/header1.xml"]);
        assert_eq!(sh.refs[0].kinds, vec!["first"]);
    }

    #[test]
    fn 没有页眉引用时结果为空() {
        let doc = doc_of(&[&[]]);
        let sh = section_headers(doc.as_bytes(), &HashMap::new());
        assert!(sh.refs.is_empty());
        assert!(sh.parts().is_empty());
    }

    #[test]
    fn 引用指向坏标识时跳过而不是瞎猜() {
        let doc = doc_of(&[&[("default", "rIdNope")]]);
        assert!(section_headers(doc.as_bytes(), &HashMap::new()).refs.is_empty());
    }

    #[test]
    fn 中间节没声明default时沿用前节而不是退回首节() {
        // 第 2 节声明了 header9；第 3 节什么都没写 → 用 header9，不是 header1
        let doc = doc_of(&[
            &[("default", "r1")],
            &[("default", "r9")],
            &[],
        ]);
        let rels = rels_of(&[
            ("r1", "word/header1.xml"),
            ("r9", "word/header9.xml"),
        ]);
        assert_eq!(
            section_headers(doc.as_bytes(), &rels).parts(),
            vec!["word/header1.xml", "word/header9.xml"]
        );
    }

    #[test]
    fn 非法type不认() {
        let doc = doc_of(&[&[("odd", "r1")]]);
        let rels = rels_of(&[("r1", "word/header1.xml")]);
        assert!(section_headers(doc.as_bytes(), &rels).refs.is_empty());
    }

    #[test]
    fn 文本片段字段与com版一致() {
        let run = text_run_xml("Hospital", "某某医院");
        assert!(run.contains(r#"w:eastAsia="宋体""#));
        assert!(run.contains(r#"w:ascii="Times New Roman""#));
        assert!(run.contains(r#"<w:sz w:val="18"/><w:szCs w:val="18"/>"#));
        assert!(run.contains(r#"<w:b w:val="0"/><w:i w:val="0"/>"#));
        assert!(run.contains(r#"<w:u w:val="none"/>"#));
        assert!(run.contains("<w:t xml:space=\"preserve\">Hospital</w:t><w:br/>"));
        // rPr 子元素顺序必须是 schema 的 sequence
        let i_font = run.find("<w:rFonts").unwrap();
        let i_b = run.find("<w:b ").unwrap();
        let i_sz = run.find("<w:sz ").unwrap();
        let i_u = run.find("<w:u ").unwrap();
        assert!(i_font < i_b && i_b < i_sz && i_sz < i_u, "rPr 子元素顺序错了：{run}");
        // 只有中文 → 不带手动换行
        assert!(!text_run_xml("", "仅有中文").contains(BR));
        assert!(!text_run_xml("only en", "").contains(BR));
    }

    #[test]
    fn 文本替换保住原段落属性() {
        let c = locate_first_cell(HDR.as_bytes()).unwrap();
        let xml = text_cell_xml(HDR.as_bytes(), &c, "EN", "中文");
        assert!(xml.contains(r#"<w:pStyle w:val="T14-logo"/>"#), "原 pStyle 要保留");
        // 段落属性块必须排在 run 前面（`w:pPr` 是 `w:p` 的第一个子元素）
        let i_ppr = xml.find("<w:pPr").expect("有 pPr");
        let i_run = xml.find("<w:r>").expect("有 run");
        assert!(i_ppr < i_run, "pPr 应当排在 run 前面：{xml}");
        // 旧的文字要没了，后面的格子不动
        assert!(!xml.contains("DOFFLON"));
        assert!(!xml.contains("后一格"));
        // 格属性原样抄回（含 hideMark 与顺序）
        assert!(xml.starts_with(r#"<w:tcPr><w:tcW w:w="2577" w:type="dxa"/><w:vAlign w:val="top"/><w:hideMark/></w:tcPr>"#));
    }

    #[test]
    fn 尺寸换算按行高contain() {
        let c = locate_first_cell(HDR.as_bytes()).unwrap();
        let im = img(800, 600);
        let (cx, cy) = picture_extent(&c, &im, true).unwrap();
        // 行高 851 twip × 635 EMU/twip = 540385 EMU，再留 10% 边距 → 486346.5 EMU。
        // 原图 800×600 px（1 px = 9525 EMU）→ 按高 contain 缩放后高 486347、宽 648462；
        // 首列 2577 twip = 1636395 EMU，宽度装得下，所以不触发列宽卡脖子。
        assert_eq!(cy, 486347, "高度应贴着行高留 10% 边距");
        assert_eq!(cx, 648462, "宽度应按原图宽高比同步缩放");
        // 不缩放时就是原尺寸
        assert_eq!(picture_extent(&c, &im, false).unwrap(), (800 * 9525, 600 * 9525));
    }

    #[test]
    fn 列宽不够时改按列宽缩放() {
        // 行高给得很大、首列很窄：应当被列宽卡住
        let xml = HDR.replace(r#"w:hRule="exact" w:val="851""#, r#"w:hRule="exact" w:val="9000""#);
        let c = locate_first_cell(xml.as_bytes()).unwrap();
        let im = img(2000, 100);
        let (cx, _cy) = picture_extent(&c, &im, true).unwrap();
        let limit = (2577i64 * EMU_PER_TWIP) as f64 * 0.9;
        assert!((cx as f64) <= limit + 1.0, "宽度应当被列宽卡住：{cx} > {limit}");
        assert!(cx > 0);
    }

    #[test]
    fn 图片模式补齐对齐与锁列() {
        let c = locate_first_cell(HDR.as_bytes()).unwrap();
        let (xml, side) = image_cell_edits(HDR.as_bytes(), &c, &img(800, 600), true, "rId7").unwrap();
        // 垂直居中：原先是 top，应当被替成 center（而且只留一个 vAlign）
        assert_eq!(xml.matches("<w:vAlign").count(), 1);
        assert!(xml.contains(r#"<w:vAlign w:val="center"/>"#), "{xml}");
        // vAlign 必须排在 hideMark 之前（schema sequence）
        assert!(xml.find("<w:vAlign").unwrap() < xml.find("<w:hideMark").unwrap());
        // 水平居中 + 段前后间距 0 + 保留 pStyle
        assert!(xml.contains(r#"<w:jc w:val="center"/>"#));
        assert!(xml.contains(r#"<w:spacing w:before="0" w:after="0"/>"#));
        assert!(xml.contains(r#"<w:pStyle w:val="T14-logo"/>"#));
        assert!(xml.contains(r#"r:embed="rId7""#));
        // 锁列：表格布局固定。`side[0]` 换的是 `w:tblPr` 的**内容**（不是单个子元素），
        // 所以要在落刀之后看结果 —— 才能验出"原来的 tblLayout 被替掉、没变成两个"。
        assert_eq!(side.len(), 1, "行高写死时不该再改行高");
        let out = apply_edits(HDR.as_bytes(), side);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains(r#"<w:tblW w:w="9356" w:type="dxa"/><w:tblLayout w:type="fixed"/>"#), "{s}");
        assert_eq!(s.matches("<w:tblLayout").count(), 1, "同名元素不许出现两个：{s}");
    }

    #[test]
    fn 自动行高时补一个至少高度() {
        let xml = HDR.replace(r#"<w:trHeight w:hRule="exact" w:val="851"/>"#, "");
        let c = locate_first_cell(xml.as_bytes()).unwrap();
        assert!(c.row_auto);
        let (_x, side) = image_cell_edits(xml.as_bytes(), &c, &img(800, 600), true, "rId1").unwrap();
        assert_eq!(side.len(), 2, "锁列 + 行高各一条");
        let h = String::from_utf8_lossy(&side[1].ins).into_owned();
        assert!(h.contains(r#"w:hRule="atLeast""#), "{h}");
        assert!(h.contains("<w:trHeight"), "{h}");
    }

    #[test]
    fn 不勾缩放时不动表格() {
        let c = locate_first_cell(HDR.as_bytes()).unwrap();
        let (xml, side) = image_cell_edits(HDR.as_bytes(), &c, &img(40, 30), false, "rId1").unwrap();
        assert!(side.is_empty(), "保持原尺寸时不该锁列/改行高");
        assert!(xml.contains(r#"cx="381000" cy="285750""#), "应当就是原尺寸：{xml}");
    }

    #[test]
    fn 生成的drawing自带命名空间() {
        let d = drawing_xml(100, 200, "rId7", "logo.png");
        for want in [
            r#"xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing""#,
            r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main""#,
            r#"xmlns:pic="#,
            r#"xmlns:r="#,
            r#"r:embed="rId7""#,
            r#"cx="100" cy="200""#,
        ] {
            assert!(d.contains(want), "缺 {want}：{d}");
        }
    }

    #[test]
    fn 内容类型表只在缺扩展名时才插() {
        let ct = br#"<Types><Default Extension="png" ContentType="image/png"/><Override PartName="/word/document.xml" ContentType="x"/></Types>"#;
        assert_eq!(ensure_default_ext(ct, "png", "image/png"), ct.to_vec(), "已有就不动");
        // 大小写不敏感（Word 有时写 "PNG"）
        assert_eq!(ensure_default_ext(ct, "PNG", "image/png"), ct.to_vec(), "已有就不动");
        let got = String::from_utf8(ensure_default_ext(ct, "gif", "image/gif")).unwrap();
        assert!(got.contains(r#"<Default Extension="gif" ContentType="image/gif"/><Override"#));
        // 不许出现两条同扩展名 —— 这正是"attr_of 只认带前缀属性"时踩过的坑
        assert_eq!(got.matches(r#"Extension="gif""#).count(), 1);
    }

    #[test]
    fn 内容类型表里没有覆写项就插在收尾标签前() {
        let ct = br#"<Types><Default Extension="png" ContentType="image/png"/></Types>"#;
        let got = String::from_utf8(ensure_default_ext(ct, "jpeg", "image/jpeg")).unwrap();
        assert!(got.ends_with(r#"<Default Extension="jpeg" ContentType="image/jpeg"/></Types>"#), "{got}");
    }

    #[test]
    fn 关系表新增与复用() {
        let old = br#"<?xml version="1.0"?><Relationships xmlns="p"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image15.jpeg"/></Relationships>"#;
        // 同一目标 → 复用
        let (rid, out) = ensure_image_rel(Some(old), "word/media/image15.jpeg");
        assert_eq!(rid, "rId1");
        assert!(out.is_none());
        // 新目标 → 新 id，且插在 `</Relationships>` 之前
        let (rid, out) = ensure_image_rel(Some(old), "word/media/wrepl-hdr-1.png");
        assert_eq!(rid, "rId2");
        let s = String::from_utf8(out.unwrap()).unwrap();
        assert!(s.contains(r#"Id="rId2""#) && s.contains("wrepl-hdr-1.png"));
        assert!(s.find("rId2").unwrap() < s.find("</Relationships>").unwrap());
        // ⚠️ 写进去的 Target 必须是**相对本部件**的 `media/…`，不是包内全路径
        //    `word/media/…` —— 后者会被解析成 `word/word/media/…`，图片指向空。
        assert!(s.contains(r#"Target="media/wrepl-hdr-1.png""#), "{s}");
        assert!(!s.contains(r#"Target="word/media/"#), "{s}");
        // 原本没有关系表 → 现建一份
        let (rid, out) = ensure_image_rel(None, "word/media/wrepl-hdr-1.png");
        assert_eq!(rid, "rId1");
        let s = String::from_utf8(out.unwrap()).unwrap();
        assert!(s.starts_with("<?xml") && s.contains("<Relationships") && s.ends_with("</Relationships>"));
        assert!(s.contains(r#"Target="media/wrepl-hdr-1.png""#), "{s}");
    }

    #[test]
    fn upsert能替换也能追加() {
        let s = r#"<w:tcW w:w="1" w:type="auto"/><w:vAlign w:val="top"/>"#;
        let g = upsert_self_closing(s, "vAlign", r#"w:val="center""#);
        assert_eq!(g.matches("<w:vAlign").count(), 1);
        assert!(g.contains(r#"<w:vAlign w:val="center"/>"#), "标签必须完整收尾：{g}");
        let h = upsert_self_closing(s, "tcW", r#"w:w="9" w:type="dxa""#);
        assert_eq!(h.matches("<w:tcW").count(), 1);
        assert!(h.contains(r#"<w:tcW w:w="9" w:type="dxa"/>"#));
        // 没有就追加；有 hideMark 时 vAlign 要排在它前面
        let k = upsert_before_marker("<w:hideMark/>", "vAlign", r#"w:val="center""#);
        assert!(k.starts_with(r#"<w:vAlign w:val="center"/>"#), "{k}");
        assert_eq!(k.matches("<w:vAlign").count(), 1);
        // 空属性串 → `<w:name/>`，不能拼出 `<w:name />` 之外的东西
        assert_eq!(upsert_self_closing("", "x", ""), "<w:x/>");
    }

    #[test]
    fn 没有属性块时补一个完整块() {
        let xml = r#"<w:hdr><w:tbl><w:tr><w:tc><w:tcPr/><w:p/></w:tc></w:tr></w:tbl></w:hdr>"#;
        let c = locate_first_cell(xml.as_bytes()).unwrap();
        assert!(c.tblpr.is_some() == false, "这份测试用的表格确实没有 tblPr");
        let ed = cell_width_lock(xml.as_bytes(), &c);
        assert_eq!(
            ed.ins,
            br#"<w:tblPr><w:tblLayout w:type="fixed"/></w:tblPr>"#.to_vec(),
            "补出来的块必须是完整、合法的 XML"
        );
        assert_eq!(ed.at, c.tbl_open_end, "块要插在父元素开标签之后");
        let out = apply_edits(xml.as_bytes(), vec![ed]);
        assert_eq!(
            String::from_utf8_lossy(&out),
            "<w:hdr><w:tbl><w:tblPr><w:tblLayout w:type=\"fixed\"/></w:tblPr><w:tr><w:tc><w:tcPr/><w:p/></w:tc></w:tr></w:tbl></w:hdr>"
        );
    }

    #[test]
    fn 属性按后缀匹配() {
        let tag = br#"<w:headerReference w:type="default" r:id="rId27"/>"#;
        assert_eq!(attr_of(tag, ":type").as_deref(), Some("default"));
        assert_eq!(attr_of(tag, ":id").as_deref(), Some("rId27"));
        // 属性值里出现 ":type" 不该被当成属性名
        assert_eq!(attr_of(br#"<x w:val="a:type=b"/>"#, ":type"), None);
        // 单引号也认
        assert_eq!(attr_of(br#"<x a='1'/>"#, ":a").as_deref(), Some("1"));
        // 非 ASCII 前缀也不能把偏移算错（这里只要不 panic、边界正确）
        assert_eq!(
            attr_of("<x专:tbl w:w=\"7\"/>".as_bytes(), ":w").as_deref(),
            Some("7")
        );
    }

    #[test]
    fn 没前缀的属性也取得到() {
        // `.rels` 里的写法（Id / Type / Target 都没有命名空间前缀）
        let rel = br#"<Relationship Id="rId7" Type="t" Target="media/a.png"/>"#;
        assert_eq!(attr_of(rel, ":Id").as_deref(), Some("rId7"));
        assert_eq!(attr_of(rel, ":Target").as_deref(), Some("media/a.png"));
        // `[Content_Types].xml` 里的写法
        let d = br#"<Default Extension="png" ContentType="image/png"/>"#;
        assert_eq!(attr_of(d, ":Extension").as_deref(), Some("png"));
        // 严格模式（key 不带 ':'）要整名相等，不能把 `w:Extension` 也算上
        assert_eq!(attr_of(d, "Extension").as_deref(), Some("png"));
        assert_eq!(attr_of(br#"<x w:a="1"/>"#, "a"), None);
        // 属性名不能从元素名里"截"出来
        assert_eq!(attr_of(br#"<x Extension="1"/>"#, ":x"), None);
    }

    #[test]
    fn 逐个属性都要解析对() {
        let t = br#"<w:tcPr a="1" b='2' c = "3" w:val="x"/>"#;
        let got = each_attr(t);
        assert_eq!(
            got,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
                ("c".to_string(), "3".to_string()),
                ("w:val".to_string(), "x".to_string()),
            ]
        );
        // 只有元素名、没有属性
        assert!(each_attr(br#"<w:p/>"#).is_empty());
        assert!(each_attr(br#"<w:p>"#).is_empty());
    }

    #[test]
    fn 页眉部件名识别() {
        assert!(is_header_part("word/header2.xml"));
        assert!(is_header_part("word/header11.xml"));
        // 页脚不归本工具管，**不算**页眉部件 —— 第一版把它们算进来了，
        // 结果真机上每份文档都多报一句"这些页脚不被任何节引用（未处理）"。
        assert!(!is_header_part("word/footer1.xml"));
        assert!(!is_header_part("word/_rels/header2.xml.rels"));
        assert!(!is_header_part("word/document.xml"));
        assert!(!is_header_part("word/media/header1.xml"));
    }

    #[test]
    fn 扫描器认边界不认子串() {
        let x = br#"<w:tblPr/><w:tblGrid><w:gridCol w:w="3"/></w:tblGrid><w:tbl><w:tr/></w:tbl>"#;
        let t = find_open(x, 0, "tbl").unwrap(); // 不能命中 tblPr / tblGrid
        assert_eq!(&x[t.start..t.start + 8], b"<w:tbl><");
        let g = find_open(x, 0, "gridCol").unwrap();
        assert_eq!(attr_num(&x[g.start..g.open_end], ":w"), Some(3));
    }

    #[test]
    fn 图片格式识别() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&120u32.to_be_bytes());
        png.extend_from_slice(&45u32.to_be_bytes());
        assert_eq!(detect(&png).unwrap(), ("png", "image/png", 120, 45));

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&240u16.to_le_bytes());
        assert_eq!(detect(&gif).unwrap(), ("gif", "image/gif", 320, 240));

        // 最小的 JPEG：SOI + SOF0(段长 17, 精度8, 高600, 宽800)
        let jpg = [
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x02, 0x58, 0x03, 0x20, 0x03, 0x01, 0x11,
            0x00, 0x02, 0x11, 0x00, 0x03, 0x11, 0x00, 0xFF, 0xD9,
        ];
        assert_eq!(detect(&jpg).unwrap(), ("jpeg", "image/jpeg", 800, 600));

        assert!(detect(b"not an image").is_err());
    }

    #[test]
    fn 落点命名不覆盖已有文件() {
        // 目录是空的 → 原名
        assert_eq!(pick_free_name("a.docx", |_| false), "a.docx");
        // 原名被占 → 加 (2)
        let taken = ["a.docx"];
        assert_eq!(pick_free_name("a.docx", |n| taken.contains(&n)), "a (2).docx");
        // (2) 也被占 → (3)
        let taken = ["a.docx", "a (2).docx"];
        assert_eq!(pick_free_name("a.docx", |n| taken.contains(&n)), "a (3).docx");
        // 多个点：扩展名按**最后一个点**切，主干里的点要保住
        let taken = ["2026-006CE.主要部件清单.docx"];
        assert_eq!(
            pick_free_name("2026-006CE.主要部件清单.docx", |n| taken.contains(&n)),
            "2026-006CE.主要部件清单 (2).docx"
        );
        // 没有扩展名
        let taken = ["无扩展名"];
        assert_eq!(pick_free_name("无扩展名", |n| taken.contains(&n)), "无扩展名 (2)");
        // 以点开头：不当成"只有扩展名"
        let taken = [".gitignore"];
        assert_eq!(pick_free_name(".gitignore", |n| taken.contains(&n)), ".gitignore (2)");
    }

    #[test]
    fn 落盘策略默认就地() {
        assert_eq!(Out::default(), Out::InPlace);
    }

    #[test]
    fn 解析真实关系表() {
        // ⚠️ 这份测试是补来的：上面那些节/页眉的测试都直接喂 `HashMap`，
        //    `parse_rels` 一个字都没被真正执行过，于是"只认带前缀的元素名"这个
        //    致命 bug 一路躲过了全部断言。真机上它的表现是"文档没有任何页眉引用"。
        let rels = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId27" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header2.xml"/>
<Relationship Id="rId28" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="/word/header3.xml"/>
<Relationship Id="rId29" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/>
</Relationships>"#;
        let m = parse_rels(rels);
        assert_eq!(m.get("rId27").map(String::as_str), Some("word/header2.xml"));
        assert_eq!(m.get("rId28").map(String::as_str), Some("word/header3.xml"));
        assert_eq!(m.get("rId29").map(String::as_str), Some("word/media/image1.png"));
        assert_eq!(m.len(), 3, "不能把注释/`<?xml` 里的名字也算成关系");
    }

    #[test]
    fn 扫描器认无前缀的元素名() {
        let x = br#"<Relationships><Relationship Id="r1" Target="a"/></Relationships>"#;
        let r = find_open(x, 0, "Relationship").expect("无前缀的元素名也要找得到");
        assert!(x[r.start..r.open_end].starts_with(br#"<Relationship Id"#));
        assert!(r.self_closing);
        // 闭标签不能被当成开标签
        let c = find_open(x, 0, "Relationships").unwrap();
        assert_eq!(&x[c.start..c.open_end], b"<Relationships>");
        // 深度计数也要认无前缀
        let y = br#"<x><y/><y>a</y></x>"#;
        assert_eq!(find_close(y, 3, "x").unwrap().start, y.len() - 4);
        // 正文/属性值里的名字不能当元素名
        assert!(find_open(br#"<a v="w:tbl">z</a>"#, 0, "tbl").is_none());
        assert!(find_open(br#"<a>Relationship</a>"#, 0, "Relationship").is_none());
    }

    #[test]
    fn 关系表里的相对与绝对目标都归一() {
        assert_eq!(resolve_target("header2.xml"), "word/header2.xml");
        assert_eq!(resolve_target("/word/header3.xml"), "word/header3.xml");
        assert_eq!(part_rels_name("word/header2.xml"), "word/_rels/header2.xml.rels");
    }

    // ─────────────────── 真机样本回归（靠环境变量开）───────────────────

    /// 取一份 docx 里所有页眉部件的字节（键是部件名）。
    fn header_parts(path: &Path) -> HashMap<String, Vec<u8>> {
        package::read_parts_where(path, |_n, k| k == PartKind::Header)
            .expect("读页眉部件")
            .into_iter()
            .filter(|(n, _, _)| is_header_part(n))
            .map(|(n, _, b)| (n, b))
            .collect()
    }

    /// 标准的 PNG CRC-32（逐位算，慢但省事 —— 只在测试里跑一次）。
    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }

    fn adler32(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &x in data {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }

    fn png_chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut crc_in = kind.to_vec();
        crc_in.extend_from_slice(body);
        let mut v = (body.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v.extend_from_slice(&crc32(&crc_in).to_be_bytes());
        v
    }

    /// 造一张**真能显示**的 PNG（纯色，RGB 真彩，deflate 用 stored 块）。
    ///
    /// 为什么不在测试里塞个 24 字节的假头就完事：真机回归的产物是要**拿 Word 打开看**的，
    /// 图片位置一个红叉会被当成"这工具坏了" —— 那是最贵的一种误判。
    fn make_png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut ihdr = w.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8 位/通道、真彩、非隔行
        let mut raw = Vec::new();
        for _ in 0..h {
            raw.push(0); // 每行开头一个滤波字节（0 = 不滤波）
            for _ in 0..w {
                raw.extend_from_slice(&rgb);
            }
        }
        let mut z = vec![0x78u8, 0x01]; // zlib 头（32K 窗口、无预设字典）
        let mut off = 0usize;
        while off < raw.len() {
            let n = (raw.len() - off).min(65535);
            let last = off + n == raw.len();
            z.push(if last { 1 } else { 0 }); // BFINAL | BTYPE=00(stored)
            z.extend_from_slice(&(n as u16).to_le_bytes());
            z.extend_from_slice(&(!(n as u16)).to_le_bytes());
            z.extend_from_slice(&raw[off..off + n]);
            off += n;
        }
        z.extend_from_slice(&adler32(&raw).to_be_bytes());

        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        out.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
        out.extend_from_slice(&png_chunk(b"IDAT", &z));
        out.extend_from_slice(&png_chunk(b"IEND", &[]));
        out
    }

    /// 拿**真实交付件**跑一遍，产物只写到临时目录，**绝不碰原件**。
    ///
    /// 开法：`WREPL_HDR_CORPUS=<含 docx 的目录> cargo test --lib 真机 -- --nocapture`
    /// 不设就跳过 —— 单调测试不依赖外部语料，这一步是**额外的**一道闸。
    ///
    /// 为什么非要有它：上面所有断言都是拿手写的小 XML 跑的。手写样本的共同毛病是
    /// "我只会写出我以为的形状"，而真实 Word 产物里的东西（`w:tblPr` 的顺序、
    /// 单元格里的嵌套表、`headerReference` 挂在哪个 `sectPr` 上、rels 用相对还是绝对
    /// 路径）恰恰是"我以为之外"的那些部分最容易出问题。
    ///
    /// 这一步**证明不了**"Word 一定打得开"（那得真拿 Word 开一次），它能证明的是：
    /// 容器完整；每个含表格的页眉部件都被改到了；不含表格的部件逐字节没动。
    #[test]
    fn 真机样本回归() {
        let Some(corpus) = std::env::var_os("WREPL_HDR_CORPUS").map(PathBuf::from) else {
            eprintln!("跳过真机样本回归：没设 WREPL_HDR_CORPUS");
            return;
        };

        let mut srcs: Vec<PathBuf> = std::fs::read_dir(&corpus)
            .unwrap_or_else(|e| panic!("读不了语料目录 {}：{e}", corpus.display()))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .map(|e| e.eq_ignore_ascii_case("docx"))
                        .unwrap_or(false)
            })
            .collect();
        srcs.sort();
        assert!(!srcs.is_empty(), "语料目录里没有 .docx：{}", corpus.display());

        // 只留"确实有可替换页眉"的样本。
        //
        // 语料里必然会混进封面、包装箱标签这类**页眉里压根没有表格**的文件 ——
        // 对它们，本工具的正确答案就是报"没有可替换的位置"（算失败，不静默跳过）。
        // 把这个当成回归失败，等于让语料的构成去改写断言。
        let total = srcs.len();
        srcs.retain(|p| {
            header_parts(p)
                .values()
                .any(|b| locate_first_cell(b).is_some())
        });
        assert!(
            !srcs.is_empty(),
            "语料目录里没有一份带表格的页眉：{}",
            corpus.display()
        );
        eprintln!(
            "语料：{total} 份，其中 {} 份的页眉里有表格（其余是封面/标签类，按规定不处理）",
            srcs.len()
        );

        // 每次跑一个**带 PID 的唯一目录**：这台机器上删除会被拦（见项目 README），
        // 复用旧目录会把上一轮的残留混进来 —— 那种"文件数凭空翻倍"的现象
        // 会伪装成代码 bug，查起来极费时间。
        let root = std::env::temp_dir().join(format!("hdr-real-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("建临时目录");

        let img_path = root.join("logo.png");
        std::fs::write(&img_path, make_png(200, 100, [0x1F, 0x6F, 0xB0])).expect("写测试图片");

        for (label, action) in [
            (
                "文本",
                Action::Text {
                    en: "WREPL E2E".to_string(),
                    cn: "小柯回归".to_string(),
                },
            ),
            (
                "图片",
                Action::Image {
                    path: img_path.clone(),
                    scale_to_table: true,
                },
            ),
        ] {
            // 每档一棵独立的输入树：`Out::SiblingOut` 固定写 `<父目录>/out`，
            // 两档共用一棵树时第二档会撞名改叫 `xxx (2).docx`，核对路径就对不上了。
            let indir = root.join(format!("in-{label}"));
            std::fs::create_dir_all(&indir).expect("建输入目录");
            let mut docs = Vec::new();
            for s in &srcs {
                let dst = indir.join(s.file_name().unwrap());
                std::fs::copy(s, &dst).expect("复制样本");
                docs.push(dst.to_string_lossy().into_owned());
            }

            let (ok, fail) = run(
                &docs,
                &action,
                Out::SiblingOut,
                &mut |s| eprintln!("  [{label}] {s}"),
                &mut |_, _| {},
            );
            eprintln!("【{label}】成功 {ok} 失败 {fail}；产物在 {}", indir.join("out").display());
            assert_eq!(fail, 0, "{label}模式有失败项，见上面的日志");
            assert_eq!(ok as usize, docs.len(), "{label}：成功数不等于样本数");

            for d in &docs {
                let src = PathBuf::from(d);
                let name = src.file_name().unwrap();
                let dst = indir.join("out").join(name);
                assert!(dst.is_file(), "{label}：没有产物 {}", dst.display());

                // ① 容器完整（`inspect` 会把每个条目解压出来算 SHA256）
                package::inspect(&dst).expect("产物不是完好的 docx 容器");
                // 文本模式一个部件都不该多、不该少；图片模式恰好多一张媒体图。
                let extra = if label == "文本" { 0 } else { 1 };
                assert_eq!(
                    package::part_names(&dst).unwrap().len(),
                    package::part_names(&src).unwrap().len() + extra,
                    "{label}：{name:?} 产物条目数不对（凭空多/少了部件）"
                );

                // ② **不该动的必须逐字节没动**：正文一个字都不是本工具的活儿，
                //    页脚同理（页眉替换只碰 word/header*.xml 与图片模式新增的部件）。
                //    这一条是"定点字节编辑"这个立身之本的最好证明 ——
                //    只要有一段偏移算错，这里立刻红。
                for keep in ["word/document.xml"] {
                    assert_eq!(
                        package::read_part(&src, keep).unwrap(),
                        package::read_part(&dst, keep).unwrap(),
                        "{label}：{name:?} 的 {keep} 被动过了（正文不该有任何改动）"
                    );
                }
                for f in package::part_names(&src).unwrap() {
                    if !f.starts_with("word/footer") || !f.ends_with(".xml") {
                        continue;
                    }
                    assert_eq!(
                        package::read_part(&src, &f).unwrap(),
                        package::read_part(&dst, &f).unwrap(),
                        "{label}：{name:?} 的 {f} 被动过了（页脚不归本工具管）"
                    );
                }

                // ③ 逐部件核对页眉
                let before = header_parts(&src);
                let after = header_parts(&dst);
                assert_eq!(before.len(), after.len(), "{label}：页眉部件数变了");
                let mut touched = 0usize;
                for (hn, was) in &before {
                    let now = after.get(hn).unwrap_or_else(|| panic!("产物里没了 {hn}"));
                    if locate_first_cell(was).is_none() {
                        assert_eq!(was, now, "{label}：{hn} 里没有表格，不该被改动");
                        continue;
                    }
                    touched += 1;
                    assert_ne!(was, now, "{label}：{hn} 应当被改，但字节没变");
                    let txt = String::from_utf8_lossy(now);
                    match label {
                        "文本" => {
                            assert!(txt.contains("WREPL E2E"), "{label} {hn} 没有英文");
                            assert!(txt.contains("小柯回归"), "{label} {hn} 没有中文");
                            assert!(
                                txt.contains(r#"w:eastAsia="宋体""#),
                                "{label} {hn} 没写东亚字体"
                            );
                        }
                        _ => {
                            assert!(txt.contains("<w:drawing>"), "{label} {hn} 里没有 drawing");
                            // 接线：页眉里的 `r:embed` 所指的 rId，在**它自己的** rels 里
                            // 必须指向本次新增的媒体部件。这一条专抓"关系表没接上"
                            // —— 字节层面看不出问题，Word 里就是一个红叉。
                            let rid = embed_rid(&txt).expect("drawing 里没有 r:embed");
                            let rn = part_rels_name(hn);
                            let rbytes =
                                package::read_part(&dst, &rn).expect("产物里读不到页眉的 rels");
                            let map = parse_rels(&rbytes);
                            let tgt = map
                                .get(&rid)
                                .unwrap_or_else(|| panic!("{rn} 里没有 {rid}"));
                            assert!(
                                tgt.starts_with("word/media/wrepl-hdr-"),
                                "{label} {hn} 的图片指向 {tgt}"
                            );
                            assert!(
                                package::part_names(&dst).unwrap().iter().any(|n| n == tgt),
                                "{label}：{tgt} 不在包里"
                            );
                        }
                    }
                }
                assert!(touched > 0, "{label}：{} 一处含表格的页眉都没找到", src.display());
            }
        }

        eprintln!("真机样本回归通过；产物保留在 {}（未删除，可自行打开核对）", root.display());
    }
}
