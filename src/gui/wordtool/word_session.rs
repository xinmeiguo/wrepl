//! Word COM 实例生命周期管理 + 通用文档批处理循环（等价 C# Core/WordSession.cs）。
//! 晚期绑定（ProgID + IDispatch），不依赖任何类型库，适用于所有 Office 版本。

use super::com::{Disp, DispResult, var_bool, var_bstr, var_i4};
use super::engine::WordKind;
use super::wd;
use windows::core::w;
use wrepl::cancel::Cancel;

pub struct WordSession {
    /// 原始 `Application` 句柄。
    ///
    /// **只给同 crate 的批处理循环与诊断探针（`wt_probe`）用** —— 它俩要直接
    /// 发 IDispatch 调用（`Documents` / `ActivePrinter` / `PrintOut` 等）。
    /// 其余代码别碰：`WordSession` 靠 `Disp` 的 `Drop` 即时 `Release`，
    /// 一旦有人 `mem::forget` 掉这个字段，Word 进程就变成孤儿了。
    pub(crate) word: Disp,
}

impl WordSession {
    /// 创建独立、不可见、禁宏、关屏刷的 Word 实例。
    pub fn create() -> DispResult<Self> {
        Self::create_as(WordKind::Word)
    }

    /// 同上，但按 [`WordKind`] 选组件：真 Word 走 `Word.Application`，
    /// WPS 走 `KWPS.Application`。**其余设置完全一致** —— 两条路都会给
    /// `Visible=false` / `DisplayAlerts=none` / `AutomationSecurity=禁宏`。
    pub fn create_as(kind: WordKind) -> DispResult<Self> {
        let progid = match kind {
            WordKind::Word => w!("Word.Application"),
            WordKind::Wps => w!("KWPS.Application"),
        };
        let w = Disp::from_progid(progid)?;
        unsafe {
            w.put("Visible", var_bool(false))?;
            w.put("DisplayAlerts", var_i4(wd::ALERTS_NONE))?;
            let _ = w.put("ScreenUpdating", var_bool(false)); // 关屏刷，后台自动化大幅加速
            let _ = w.put(
                "AutomationSecurity",
                var_i4(wd::AUTOMATION_SECURITY_FORCE_DISABLE),
            ); // 禁宏
        }
        Ok(WordSession { word: w })
    }

    /// 彻底关闭 Word 并释放 COM 引用（等价 Quit(不保存) + ReleaseComObject）。
    /// Disp 的 Drop 即时 Release，无需 C# 的两次 GC 强制回收。
    pub fn cleanup(&mut self) {
        unsafe {
            let _ = self.word.call_named(
                "Quit",
                vec![("SaveChanges", var_i4(wd::DO_NOT_SAVE_CHANGES))],
            );
        }
    }
}

fn file_name(p: &str) -> &str {
    p.rsplit(['\\', '/']).next().unwrap_or(p)
}

/// 「临时切换 Word 的全局默认打印机」这件事的守卫。
///
/// `ActivePrinter` 不是文档属性，是 **Word 应用级设置**，落在注册表里 ——
/// 换了不换回来，用户以后从 Word 里点「打印」就默认跑到我们选的那台去，
/// 而他根本不知道是谁改的。
///
/// 正常路径由 [`restore`](PrinterGuard::restore) 显式收尾（能打日志、能补端口、
/// 能汇报失败）。但 `ActivePrinter` 最危险的时候恰恰是**没走正常路径**：
/// 批处理中途 panic，或某处 `?` 提前返回 —— 那时函数末尾那几行恢复代码根本跑不到。
/// `Drop` 是唯一能覆盖这两种情况的钩子，所以恢复逻辑必须**同时**挂在这里
/// （Drop 里拿不到 `log`，也不再读打印机列表补端口：尽力换回裸名字即可，
/// 而裸名字 Word 是收的，已实测）。
struct PrinterGuard<'a> {
    word: &'a Disp,
    /// 切换前的原值；`None` = 没切过 / 已经恢复过。
    previous: Option<String>,
    /// 显式恢复过了吗？恢复过 `Drop` 就不再碰，免得把用户自己又改的值覆盖掉。
    restored: bool,
}

impl<'a> PrinterGuard<'a> {
    /// `previous` 是**切换前**读到的原值（读不到就传 `None`：没有可恢复的东西）。
    fn new(word: &'a Disp, previous: Option<String>) -> Self {
        Self {
            restored: previous.is_none(),
            word,
            previous,
        }
    }

    /// 正常收尾。失败只说一句、**不改返回值** —— 打印本身已经完成了，
    /// 不该因为"没恢复成"就把整批算作失败。
    fn restore(&mut self, log: &mut dyn FnMut(String)) {
        if self.restored {
            return;
        }
        self.restored = true;
        let Some(prev) = self.previous.take() else {
            return;
        };
        // 实测（2026-10-10，本机 MS Word）：`ActivePrinter` 读回来往往是**裸名字**
        // （`"ApeosPort-V C4475 T2"`），而不是老文档里写的 `"Name on Ne01:"`。
        // 裸名字 Word 也收（已实测），但带端口的写法才是文档记载的规范形状，
        // 所以能补上端口就补上 —— 兼容"只认带端口写法"的版本。
        let restore = if prev.contains(" on ") {
            prev
        } else {
            super::printers::list()
                .ok()
                .and_then(|l| {
                    l.into_iter()
                        .find(|x| x.name.eq_ignore_ascii_case(&prev))
                })
                .map(|x| x.active_string())
                .unwrap_or(prev)
        };
        if let Err(e) = unsafe { self.word.put("ActivePrinter", var_bstr(&restore)) } {
            log(format!("提示：恢复原打印机失败（{restore}，{e}）"));
        } else {
            log(format!("已恢复原打印机：{restore}"));
        }
    }
}

impl Drop for PrinterGuard<'_> {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        if let Some(prev) = self.previous.take() {
            let _ = unsafe { self.word.put("ActivePrinter", var_bstr(&prev)) };
        }
    }
}

/// 通用批处理骨架：对每个文档执行 action，自动完成 打开 → 处理 → (可选)保存 → 关闭 → 释放 → 日志。
/// 单个文档失败不影响后续文档；无论成败，结束后都彻底清理 Word 进程。
/// `progress(current, total)` 在每个文档开始处理时回传（current 从 1 开始）。
///
/// ## action 的两个参数与返回值
///
/// * `&Disp` —— 已打开的文档；`&str` —— **它的源路径**（导出 PDF 这类
///   "产物名由源文件名派生"的动作需要它，否则只能退回把路径塞进闭包的捕获里，
///   变成"每个文档都算同一个目标"）。
/// * 返回 `Ok(Some(备注))` 时，备注会接在 `"{done_verb}：文件名"` 后面一起写进日志
///   （导出 PDF 用它报产物落到哪、有没有因为撞名改存）。普通动作返回 `Ok(None)`。
pub fn run_on_documents<F>(
    kind: WordKind,
    doc_paths: &[String],
    action: F,
    log: &mut dyn FnMut(String),
    progress: &mut dyn FnMut(usize, usize),
    save_after: bool,
    done_verb: &str,
    cancel: Option<&Cancel>,
) -> (i32, i32)
where
    F: Fn(&Disp, &str) -> DispResult<Option<String>>,
{
    let mut session = match WordSession::create_as(kind) {
        Ok(s) => s,
        Err(e) => {
            log(format!("启动 {} 失败：{e}", kind.label_cn()));
            return (0, doc_paths.len() as i32);
        }
    };
    let r = batch_loop(
        &session.word,
        doc_paths,
        action,
        log,
        progress,
        save_after,
        done_verb,
        cancel,
    );
    session.cleanup();
    r
}

/// 与 [`run_on_documents`] 相同，但整批跑在**指定打印机**上。
///
/// 这一条**始终用 Microsoft Word**（[`WordKind::Word`]）：WPS 的 `PrintOut`
/// 没有实测过，而"打到纸上"是不可撤销的动作，不能拿没验过的路径去赌。
/// 只有 WPS 的机器上，界面会把「打印机」这一档直接拦下来并提示改走导出，见
/// [`super::engine::PRINT_NEEDS_WORD`]。
///
/// `printer` 是 `None` 就等价于 `run_on_documents`（用系统默认那台）。
/// 给了就先把 Word 的 `ActivePrinter` 切过去，**整批跑完再恢复原样** ——
/// 这个属性是 Word 的全局设置（落在注册表里），不改回去等于偷偷改了用户
/// 以后从 Word 里打印的默认去处。
///
/// 切不过去时**直接放弃整批**（返回 `(0, 总数)`）而不是"退回默认打印机接着打"：
/// 用户是明确点了某台机器的，打到别处去比不打出错更严重 —— 几十份纸质件
/// 打到错误的打印机上，纸和时间都追不回来。
pub fn run_on_documents_printing<F>(
    doc_paths: &[String],
    action: F,
    log: &mut dyn FnMut(String),
    progress: &mut dyn FnMut(usize, usize),
    save_after: bool,
    done_verb: &str,
    printer: Option<&str>,
    cancel: Option<&Cancel>,
) -> (i32, i32)
where
    F: Fn(&Disp, &str) -> DispResult<Option<String>>,
{
    let mut session = match WordSession::create() {
        Ok(s) => s,
        Err(e) => {
            log(format!("启动 Word 失败：{e}"));
            return (0, doc_paths.len() as i32);
        }
    };

    // 换打印机的第一步是**记住原来那台** —— 否则无从恢复。
    let previous: Option<String> = match &printer {
        Some(_) => match unsafe { session.word.get("ActivePrinter") } {
            Ok(v) => unsafe { super::com::res_string(v) }.ok(),
            Err(_) => None,
        },
        None => None,
    };

    if let Some(want) = printer {
        if let Err(e) = unsafe { session.word.put("ActivePrinter", var_bstr(want)) } {
            log(format!("切换打印机失败：{want}（{e}）—— 已放弃本次打印"));
            session.cleanup();
            return (0, doc_paths.len() as i32);
        }
        log(format!("已选用打印机：{want}"));
    }

    // ★ 守卫在**切换之后**建：它拿到的 `previous` 是切换前读到的值，
    //   之后任何一条退出路径 —— 正常跑完、`batch_loop` 里 panic、某处 `?` 提前
    //   返回 —— 都会走到它的 `Drop`，轮到那条路径也把用户的默认打印机换回去。
    let mut guard = PrinterGuard::new(&session.word, previous);

    let r = batch_loop(
        &session.word,
        doc_paths,
        action,
        log,
        progress,
        save_after,
        done_verb,
        cancel,
    );

    // 正常收尾：这里能打日志、能补端口，优先于 `Drop` 的裸名字兜底。
    guard.restore(log);
    // 显式 drop 结束对 `session.word` 的借用（下面 `cleanup()` 要 `&mut`）。
    // `restored` 已置位，这次 Drop 不会再碰打印机。
    drop(guard);
    session.cleanup();
    r
}

/// 真正的批处理循环。抽出来是为了让上面两条入口（换打印机 / 不换）
/// **共用同一段逻辑** —— 复制一份出去迟早会两边走偏。
fn batch_loop<F>(
    word: &Disp,
    doc_paths: &[String],
    action: F,
    log: &mut dyn FnMut(String),
    progress: &mut dyn FnMut(usize, usize),
    save_after: bool,
    done_verb: &str,
    cancel: Option<&Cancel>,
) -> (i32, i32)
where
    F: Fn(&Disp, &str) -> DispResult<Option<String>>,
{
    let total = doc_paths.len();
    let (mut ok, mut fail) = (0i32, 0i32);
    for (idx, path) in doc_paths.iter().enumerate() {
        // 取消只在**文档边界**生效：已经在 Word 里打开的那个一定跑完再关，
        // 后面的不再开始。半路停会留下一个开着的文档和写了一半的 PDF ——
        // 那种残局比"这个文件没处理"难收拾得多。
        if cancel.is_some_and(|c| c.is_cancelled()) {
            log(format!(
                "已取消：剩余 {} 个文档未处理（本批已处理 {ok} 个 / 失败 {fail} 个）",
                total - idx
            ));
            break;
        }
        progress(idx + 1, total);
        // ★ 进 Word 之前先自己验一遍这个包能不能解析。
        //   Word 遇到损坏文档会弹「文档修复」模态框，而批处理里 Word 是不可见的
        //   —— 那个框没人点得到，整批就卡在那儿等，用户只能杀进程。
        //   zip 级损坏（截断、改名的 .doc、空文件）是这类弹框的主力，我们能拦就拦。
        if let Err(why) = preflight(path) {
            fail += 1;
            log(format!("跳过（文档本身打不开）：{path}（{why}）"));
            continue;
        }
        unsafe {
            let opened: DispResult<Disp> = (|| {
                let docs = super::com::res_disp(word.get("Documents")?)?;
                super::com::res_disp(docs.call("Open", vec![var_bstr(path)])?)
            })();
            match opened {
                Err(e) => {
                    fail += 1;
                    log(format!("处理出错，跳过：{path}（{e}）"));
                }
                Ok(doc) => {
                    let mut failure: Option<String> = None;
                    let mut note: Option<String> = None;
                    match action(&doc, path) {
                        Err(e) => failure = Some(e),
                        Ok(n) => {
                            note = n;
                            if save_after {
                                if let Err(e) = doc.callv("Save", vec![]) {
                                    failure = Some(e);
                                }
                            }
                        }
                    }
                    match failure {
                        Some(e) => {
                            fail += 1;
                            log(format!("处理出错，跳过：{path}（{e}）"));
                        }
                        None => {
                            ok += 1;
                            // `note` 由动作自己给（导出 PDF 用它报产物落点）；
                            // 普通动作是 `None`，日志与改动前一字不差。
                            let tail = note.unwrap_or_default();
                            log(format!("{done_verb}：{}{tail}", file_name(path)));
                        }
                    }
                    let _ = doc.callv("Close", vec![var_bool(false)]);
                    drop(doc); // 即时 Release
                }
            }
        }
    }
    (ok, fail)
}

/// 进 Word 之前的自查：这个文件能不能当 docx 解析。
///
/// ## 为什么必须有这一步
///
/// Word 遇到损坏文档会弹**「文档修复」模态框**，而批处理里 Word 是不可见的
/// （`Visible = false`）—— 那个框用户根本点不到，整批就停在 `Documents.Open`
/// 里一直等，界面上显示的是"正在处理第 N 个"，实际已经死在那儿了。
/// `DisplayAlerts = 0` **挡不住**这个框：它不属于普通提示框。
///
/// 而"这个文件能不能打开"我们**自己就能判断**，不必交给 Word 去撞。
///
/// ## 只做到 zip 这一层
///
/// **不做** XML 语义校验。理由是代价不对等：一次完整 XML 解析是 10 ms 级，
/// 而打印/导出的单件耗时本来就是秒级（Word 打开一个文档就要 1–3 秒），
/// 多花那 10 ms 省不下什么；而 zip 级损坏（截断、改名的 `.doc`、空文件、
/// 网盘同步到一半）恰好是"Word 弹修复框"的主力。
///
/// 剩下的那部分（zip 完好但 XML 坏了）拦不住。真正的兜底是**取消按钮**：
/// 用户看到卡住能自己停，而不是只能杀进程。
fn preflight(path: &str) -> Result<(), String> {
    let names = wrepl::docx::package::part_names(std::path::Path::new(path))
        .map_err(|e| format!("{e:#}"))?;
    if names.iter().any(|n| n == "[Content_Types].xml") {
        Ok(())
    } else {
        Err("缺 [Content_Types].xml，不是有效的 docx".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::preflight;
    use std::path::PathBuf;

    /// 建在测试二进制旁边（本机 `%TEMP%` 在 C 盘，对非系统进程不是稳定可写）。
    fn scratch(name: &str) -> PathBuf {
        let exe = std::env::current_exe().expect("取当前测试可执行文件路径");
        let dir = exe
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(exe.as_path())
            .join("wrepl-wordsession-tmp");
        std::fs::create_dir_all(&dir).expect("建测试临时目录");
        dir.join(name)
    }

    /// 非 zip 必须被拦在 Word 门外。
    ///
    /// 这条钉的是"别等 Word 去发现"：Word 发现的方式是弹一个**不可见**的
    /// 修复模态框（批处理里 `Visible = false`），整批就停在 `Open` 里一直等，
    /// 而 `DisplayAlerts = 0` 挡不住它。我们自己先看一眼，代价是几毫秒。
    ///
    /// 注意它**测不到**"zip 完好但 XML 坏了"那种 —— 那一类真拦不住，
    /// 靠界面上的「取消」按钮兜底。别把这条当成万灵药。
    #[test]
    fn 预检拦住不是_zip_的文件() {
        let p = scratch("broken.docx");
        std::fs::write(&p, b"not a zip at all").expect("写测试文件");

        let err = preflight(&p.display().to_string()).expect_err("非 zip 应当被拦下");
        assert!(!err.is_empty(), "拦下时要给出能看懂的原因");
    }

    /// 文件根本不存在时也应当是"拦下 + 说明"，不能 panic。
    #[test]
    fn 预检拦住不存在的文件() {
        let p = scratch("no-such-file.docx");
        let _ = std::fs::remove_file(&p);
        assert!(preflight(&p.display().to_string()).is_err());
    }
}
