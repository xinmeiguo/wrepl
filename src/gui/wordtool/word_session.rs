//! Word COM 实例生命周期管理 + 通用文档批处理循环（等价 C# Core/WordSession.cs）。
//! 晚期绑定（ProgID + IDispatch），不依赖任何类型库，适用于所有 Office 版本。

use super::com::{Disp, DispResult, var_bool, var_bstr, var_i4};
use super::wd;
use windows::core::w;

pub struct WordSession {
    pub word: Disp,
}

impl WordSession {
    /// 创建独立、不可见、禁宏、关屏刷的 Word 实例。
    pub fn create() -> DispResult<Self> {
        let w = Disp::from_progid(w!("Word.Application"))?;
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

/// 通用批处理骨架：对每个文档执行 action，自动完成 打开 → 处理 → (可选)保存 → 关闭 → 释放 → 日志。
/// 单个文档失败不影响后续文档；无论成败，结束后都彻底清理 Word 进程。
/// `progress(current, total)` 在每个文档开始处理时回传（current 从 1 开始）。
pub fn run_on_documents<F>(
    doc_paths: &[String],
    action: F,
    log: &mut dyn FnMut(String),
    progress: &mut dyn FnMut(usize, usize),
    save_after: bool,
    done_verb: &str,
) -> (i32, i32)
where
    F: Fn(&Disp) -> DispResult<()>,
{
    let mut session = match WordSession::create() {
        Ok(s) => s,
        Err(e) => {
            log(format!("启动 Word 失败：{e}"));
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
    );
    session.cleanup();
    r
}

/// 与 [`run_on_documents`] 相同，但整批跑在**指定打印机**上。
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
) -> (i32, i32)
where
    F: Fn(&Disp) -> DispResult<()>,
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

    let r = batch_loop(
        &session.word,
        doc_paths,
        action,
        log,
        progress,
        save_after,
        done_verb,
    );

    // 恢复原打印机。**失败只说一句，不改返回值** —— 打印本身已经完成了，
    // 不该因为"没恢复成"就把整批算作失败。
    if printer.is_some() {
        if let Some(prev) = previous {
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
            if let Err(e) = unsafe { session.word.put("ActivePrinter", var_bstr(&restore)) } {
                log(format!("提示：恢复原打印机失败（{restore}，{e}）"));
            } else {
                log(format!("已恢复原打印机：{restore}"));
            }
        }
    }

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
) -> (i32, i32)
where
    F: Fn(&Disp) -> DispResult<()>,
{
    let total = doc_paths.len();
    let (mut ok, mut fail) = (0i32, 0i32);
    for (idx, path) in doc_paths.iter().enumerate() {
        progress(idx + 1, total);
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
                    if let Err(e) = action(&doc) {
                        failure = Some(e);
                    } else if save_after {
                        if let Err(e) = doc.callv("Save", vec![]) {
                            failure = Some(e);
                        }
                    }
                    match failure {
                        Some(e) => {
                            fail += 1;
                            log(format!("处理出错，跳过：{path}（{e}）"));
                        }
                        None => {
                            ok += 1;
                            log(format!("{done_verb}：{}", file_name(path)));
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
