//! 界面状态持久化：主题 + 规则表 + 替换开关 + 「打印与导出」设置。
//!
//! ## 存哪儿、为什么不跟日志混在一起
//!
//! `%LOCALAPPDATA%\wrepl\state.json` —— 与 `wrepl-gui.log`、`last-dir.txt` 同目录
//! （见 [`crate::diag::state_dir`]）。日志会被轮转、会被删；状态是**用户的东西**，
//! 混进日志迟早一起丢。所以单独一个文件，且**格式是人类可读的 JSON**：
//! 出问题时能直接打开看，必要时手改也能救回来。
//!
//! ## 三条硬规矩
//!
//! 1. **读失败一律当"没有"**。文件不存在、被手改坏、磁盘拔了 —— 都不许让程序
//!    起不来（同 [`crate::diag::load_last_dir`] 的取向）。拿不到就用默认值。
//! 2. **字段级容错**。整份结构挂了 `#[serde(default)]`：少一个字段就用该字段的
//!    默认值，不整份作废。这样将来加字段，旧文件仍然读得进来。
//! 3. **写是"先写临时文件再改名"**。写到一半被杀掉时，磁盘上留下的仍是上一份
//!    完整状态，而不是一个截断的、下次读不出来的半截文件。
//!
//! ## 存什么、不存什么
//!
//! 存：**重打一遍很费手的东西** —— 整张规则表（含作用域与三个匹配开关）、
//! 替换的几个开关（链式 / 最长匹配 / 同步改名 / 保留 .bak / 递归 / 排除）、
//! 主题、「打印与导出」页的下游（打印还是 PDF）、页码范围、PDF 目录、PDF/A、
//! 选中的打印机。
//!
//! 不存：**输入 / 输出路径**。理由是它最容易变成一条点不回来的死路径（盘没插、
//! 目录被移走），而"上次停在哪个目录"这件事已经由 `last-dir.txt` 负责；
//! 也不存窗口大小位置 —— 那归窗口管理器，存了反而会让"上次是什么样"变成猜谜。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 状态文件格式版本。将来真要改结构，靠它决定走不走迁移。
pub const VERSION: u32 = 1;

/// 整份界面状态。
///
/// 所有字段都允许缺失（容器级 `#[serde(default)]`），缺失时取 [`Default`]。
/// 取向：**宁可少恢复几项，也不要因为一个陌生字段让整份状态读不出来。**
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub version: u32,
    /// `"light"` / `"dark"`。
    ///
    /// 存字符串而不是枚举：文件被手改成不认识的词时，只丢这一项，
    /// 而不是整份 `from_str` 失败、连规则表一起作废（枚举遇到未知变体会这样）。
    pub theme: String,
    pub rules: Vec<RuleState>,
    pub replace: ReplaceState,
    pub print: PrintState,
}

impl Default for State {
    fn default() -> Self {
        State {
            version: VERSION,
            theme: "light".to_string(),
            rules: Vec::new(),
            replace: ReplaceState::default(),
            print: PrintState::default(),
        }
    }
}

impl State {
    pub fn is_dark(&self) -> bool {
        self.theme.eq_ignore_ascii_case("dark")
    }
}

/// 规则表里的一行。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleState {
    pub find: String,
    pub replace: String,
    /// 作用域串（`全部` / `正文,页眉页脚`…）。界面不显示，随规则往返。
    pub scope: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub kana_sensitive: bool,
    pub note: String,
}

impl Default for RuleState {
    fn default() -> Self {
        RuleState {
            find: String::new(),
            replace: String::new(),
            // 与界面 `Row::new` 的默认一致：作用域为空＝全部
            scope: String::new(),
            case_sensitive: false,
            whole_word: false,
            kana_sensitive: false,
            note: String::new(),
        }
    }
}

/// 「批量替换」页的开关。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplaceState {
    pub chain: bool,
    pub longest_first: bool,
    pub rename_files: bool,
    pub backup: bool,
    pub recursive: bool,
    pub exclude: String,
}

impl Default for ReplaceState {
    fn default() -> Self {
        // ★ 必须与 `App::new` 的默认值逐项对齐：这里的默认值会在
        //   ① 首次启动（还没有 state.json）、② 旧文件缺这个字段 时生效。
        //   对不上就会造成"第一次打开是一个样、第二次打开是另一个样"。
        ReplaceState {
            chain: false,
            longest_first: false,
            rename_files: true,
            backup: false,
            recursive: false,
            exclude: String::new(),
        }
    }
}

/// 「打印与导出」页的设置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrintState {
    /// `"printer"` / `"pdf"`。存字符串，理由同 [`State::theme`]。
    pub sink: String,
    pub page_range: String,
    pub pdf_dir: String,
    pub pdf_a: bool,
    /// 选中的打印机**名字**（不用下标：列表顺序会随系统变化）。
    /// 空串 = 用系统默认。
    pub printer: String,
}

impl Default for PrintState {
    fn default() -> Self {
        PrintState {
            sink: "printer".to_string(),
            page_range: String::new(),
            pdf_dir: String::new(),
            pdf_a: false,
            printer: String::new(),
        }
    }
}

impl PrintState {
    pub fn is_pdf(&self) -> bool {
        self.sink.eq_ignore_ascii_case("pdf")
    }
}

/// 状态文件路径。
pub fn path() -> Option<PathBuf> {
    Some(crate::diag::state_dir()?.join("state.json"))
}

/// 读状态。**任何失败都返回 `None`**，并把原因记进日志。
pub fn load() -> Option<State> {
    let p = path()?;
    let st = load_from(&p)?;
    crate::diag::log(format!(
        "已读回界面状态：{} 条规则（{}）",
        st.rules.len(),
        p.display()
    ));
    Some(st)
}

/// 落盘。返回给人看的错误文本（调用方只记日志，不弹窗）。
pub fn save(st: &State) -> Result<(), String> {
    let p = path().ok_or_else(|| "取不到状态目录（LOCALAPPDATA）".to_string())?;
    save_to(&p, st)
}

fn load_from(p: &Path) -> Option<State> {
    let raw = std::fs::read_to_string(p).ok()?;
    match serde_json::from_str::<State>(&raw) {
        Ok(st) => Some(st),
        Err(e) => {
            // 坏文件不删：万一里面有用户手写的东西，留着让他自己看。
            crate::diag::log(format!("界面状态读不出来（按默认值继续）：{e}"));
            None
        }
    }
}

/// 先写 `state.json.tmp` 再改名。见模块头第 3 条。
fn save_to(p: &Path, st: &State) -> Result<(), String> {
    let text = serde_json::to_string_pretty(st)
        .map_err(|e| format!("状态序列化失败：{e}"))?;
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, text.as_bytes())
        .map_err(|e| format!("写 {} 失败：{e}", tmp.display()))?;
    // Windows 上 `rename` 目标已存在时是**覆盖**（内部走 MOVEFILE_REPLACE_EXISTING），
    // 所以这里不需要先删旧文件 —— 少一步，也少一个"删掉了但没写进去"的窗口。
    std::fs::rename(&tmp, p).map_err(|e| format!("替换 {} 失败：{e}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        // 同 `report` / `verify` 的测试：不用 `%TEMP%`（本机在 C 盘，不是稳定可写），
        // 而是放在测试二进制旁边。
        let exe = std::env::current_exe().expect("取当前测试可执行文件路径");
        let dir = exe
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(exe.as_path())
            .join("wrepl-state-tmp");
        std::fs::create_dir_all(&dir).expect("建测试临时目录");
        dir.join(name)
    }

    /// 少字段的旧文件要能读进来，且缺的那项取**界面默认值**（不是 `false`）。
    ///
    /// 这条钉的是"加了字段之后旧 state.json 仍然可用"：`rename_files` 界面默认
    /// 是**勾上**，若这里退化成 `false`，用户会以为自己的勾选被改了。
    #[test]
    fn 旧文件缺字段时取界面默认值() {
        let raw = r#"{"version":1,"theme":"dark","rules":[{"find":"冻干机","replace":"清洗机"}]}"#;
        let st: State = serde_json::from_str(raw).expect("缺字段不该整份作废");
        assert!(st.is_dark());
        assert_eq!(st.rules.len(), 1);
        assert_eq!(st.rules[0].find, "冻干机");
        assert_eq!(st.rules[0].scope, "", "缺的作用域留空＝全部");
        assert!(st.replace.rename_files, "缺 rename_files 必须取界面默认（勾上）");
        assert!(!st.replace.chain);
        assert!(!st.print.is_pdf(), "缺 sink 默认送打印机");
        assert_eq!(st.print.printer, "");
    }

    /// 不认识的主题 / 下游词只丢那一项，不连累整份状态。
    #[test]
    fn 陌生枚举词只影响自己那一项() {
        let raw = r#"{"theme":"紫色","print":{"sink":"传真"},"rules":[{"find":"a","replace":"b"}]}"#;
        let st: State = serde_json::from_str(raw).expect("未知字符串不该让解析失败");
        assert!(!st.is_dark(), "认不出来就按浅色");
        assert!(!st.print.is_pdf(), "认不出来就按送打印机");
        assert_eq!(st.rules.len(), 1, "规则表照常读进来");
    }

    /// 坏 JSON → `None`，不 panic。
    #[test]
    fn 坏文件读成没有() {
        let p = scratch("broken.json");
        std::fs::write(&p, b"{ this is not json ").expect("写测试文件");
        assert!(load_from(&p).is_none());
        assert!(load_from(&scratch("not-there.json")).is_none());
    }

    /// 转义往返：引号、反斜杠、换行、制表符、中日文、emoji 一个都不许变。
    ///
    /// 这些字符正是"手写行格式"会翻车的地方（也是引入 serde_json 的全部理由）。
    #[test]
    fn 自由文本原样往返() {
        let st = State {
            theme: "dark".into(),
            rules: vec![
                RuleState {
                    find: "引号\" 反斜杠\\ 换行\n 制表\t 控制\u{1}".into(),
                    replace: "清洗机（CIP）　全角空格".into(),
                    scope: "正文,页眉页脚".into(),
                    case_sensitive: true,
                    whole_word: true,
                    kana_sensitive: true,
                    note: "备注 emoji 🧪 也要活下来".into(),
                },
                RuleState::default(),
            ],
            replace: ReplaceState {
                chain: true,
                rename_files: false,
                exclude: "*.bak;临时/**".into(),
                ..Default::default()
            },
            print: PrintState {
                sink: "pdf".into(),
                page_range: "1-3,5".into(),
                pdf_dir: "D:/交付/pdf".into(),
                pdf_a: true,
                printer: "\\\\服务器\\激光打印机".into(),
            },
            ..Default::default()
        };

        let text = serde_json::to_string_pretty(&st).expect("序列化");
        let back: State = serde_json::from_str(&text).expect("反序列化");
        assert_eq!(st, back, "往返后必须逐字段相同");
    }

    /// 写盘 → 读回，且临时文件不留在原地。
    #[test]
    fn 写盘后能读回且不留临时文件() {
        let p = scratch("state.json");
        let st = State {
            rules: vec![RuleState {
                find: "LYO".into(),
                replace: "APS".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        save_to(&p, &st).expect("写状态");
        assert_eq!(load_from(&p).as_ref(), Some(&st));
        let tmp = p.with_extension("json.tmp");
        assert!(!tmp.exists(), "临时文件必须已经被改名走，不该留在目录里");

        // 第二次写要能覆盖（Windows 上 rename 覆盖已存在文件的语义）
        let st2 = State {
            theme: "dark".into(),
            ..Default::default()
        };
        save_to(&p, &st2).expect("覆盖写状态");
        assert_eq!(load_from(&p).as_ref(), Some(&st2));
    }
}
