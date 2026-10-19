//! 探测本机能用哪套 Word 兼容引擎，以及"降级用 WPS"要不要先问一声。
//!
//! ## 为什么要专门探测
//!
//! 「打印与导出」整页都挂在 Word 的 COM 上。中文环境里**只装 WPS、没装
//! Microsoft Word** 的机器很常见，而 WPS 也注册了一个接口高度相似的
//! `KWPS.Application` —— 能跑，但**排版结果不一样**：同一批交付件实测，
//! WPS 每份都比 Word 少 1 页（23→22、10→9、27→26，3/3 复现），页脚的
//! 「Page N of 23」跟着变成「of 22」。详见 `CHANGELOG.md` 的对照表。
//!
//! 所以这里**不做静默降级**。本模块只回答两个问题：
//!
//! 1. 现在这台机器上，真正的 Word 在不在？（[`EngineProbe::detect`]）
//! 2. 只能在 WPS 上跑时，要不要先让使用者点一下确认？（[`needs_wps_confirm`]）
//!
//! ## ★ 为什么不能只看「`Word.Application` 注册了没有」
//!
//! WPS 在"设为默认"或兼容模式下**会把自己注册成 `Word.Application`**。
//! 那时候 ProgID 存在、`CoCreateInstance` 也能成功，但拿到的是 WPS 的排版引擎
//! —— 只看 ProgID 存在就会**悄悄用 WPS 出一个少一页的 PDF**，正是我们要防的。
//!
//! 所以判据是**CLSID 挂到哪个 exe 上**：`Word.Application\CLSID` →
//! `CLSID\{…}\LocalServer32` → 文件名必须是 `WINWORD.EXE`，否则不算 Word。

use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, KEY_READ, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW,
};

/// 实际去驱动 COM 的那套引擎。ProgID 是它唯一的差别 —— 其余调用完全相同。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WordKind {
    /// Microsoft Word（`Word.Application`）。
    Word,
    /// WPS 文字（`KWPS.Application`）。**排版结果与 Word 不同**。
    Wps,
}

impl WordKind {
    /// 给人看的名字（日志与失败提示里用）。
    pub fn label_cn(self) -> &'static str {
        match self {
            WordKind::Word => "Microsoft Word",
            WordKind::Wps => "WPS 文字",
        }
    }
}

/// 本机的引擎状况。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EngineProbe {
    /// 有真正的 Microsoft Word —— 行为与以前一致，不做任何额外提示。
    Word,
    /// 没有 Word，但有 WPS。能用，**分页会变**，用之前必须问一声。
    WpsOnly,
    /// 两样都没有 —— 本页做不了，按钮该灰掉并说明原因。
    None,
}

impl EngineProbe {
    /// 探测本机。**只读注册表**，不启动 Word / WPS，几微秒返回，
    /// 放在界面线程上调用是安全的。
    pub fn detect() -> Self {
        classify(word_local_server().as_deref(), progid_registered("KWPS.Application"))
    }

    /// 给人看的判定结论（界面上那行小字用）。
    pub fn label_cn(self) -> &'static str {
        match self {
            EngineProbe::Word => "Microsoft Word",
            EngineProbe::WpsOnly => "WPS 文字（降级）",
            EngineProbe::None => "未检测到 Word / WPS",
        }
    }
}

/// 确认框正文。**只留能让人做判断的那一条**：分页差异 + 实测数字。
/// 展开写"表格换行也可能动""PDF/A 未验证"只会把重点冲淡 —— 详细对照在
/// `CHANGELOG.md`，界面不重复。
pub const WPS_CONFIRM_BODY: &str =
    "WPS 与 Word 排版不同：每份少 1 页（页脚 of 23→of 22），PDF 无 Tagged 标签。";

/// 「打印」这一档在只有 WPS 时的拒绝理由。
pub const PRINT_NEEDS_WORD: &str = "打印需要 Microsoft Word。可改用「导出 PDF」（会先确认）。";

/// 「装什么都没装」时的拒绝理由。
pub const NO_ENGINE: &str = "未检测到 Microsoft Word / WPS，装一个再用。";

/// 只有 WPS 时，页面上那条常驻横幅。
///
/// **写在点按钮之前**：等人家点了才说，等于让人先以为"导出"是个安全的动作。
pub const WPS_ONLY_BANNER: &str = "本机只有 WPS 文字：导出按 WPS 排版（分页会变），打印不可用。";

/// 纯判断：`word_srv` 是 `Word.Application` 那个 CLSID 的 `LocalServer32`
/// 命令行（取不到就是 `None`），`wps_registered` 是 `KWPS.Application` 在不在。
///
/// 抽成纯函数是为了能对着假数据测 —— 真注册表只有一台，改不动。
pub fn classify(word_srv: Option<&str>, wps_registered: bool) -> EngineProbe {
    match word_srv {
        Some(cmd) if is_winword(cmd) => EngineProbe::Word,
        // ProgID 存在但指向的不是 WINWORD.EXE（典型：WPS 把自己注册成
        // Word.Application）—— 这**不算**有 Word，按"只有 WPS"处理。
        _ if wps_registered => EngineProbe::WpsOnly,
        _ => EngineProbe::None,
    }
}

/// `LocalServer32` 的值形如 `C:\...\Office16\WINWORD.EXE /Automation`
/// （带参数、且路径空格没有引号）—— 取最后一段的文件名再比。
fn is_winword(cmd: &str) -> bool {
    exe_name(cmd).eq_ignore_ascii_case("winword.exe")
}

/// 从 `LocalServer32` 的值里抠出可执行文件名。
///
/// ★ 这里有个真踩过的坑：Word 的值长这样
/// `C:\…\Office16\WINWORD.EXE /Automation` —— **参数用的是正斜杠**。
/// 直接按 `\` 或 `/` 取最后一段，会拿到 `Automation` 而不是 `WINWORD.EXE`，
/// 于是"有 Word"被误判成"没有 Word"（这条被单元测试当场抓住）。
///
/// 正确顺序是**先去参数、再取文件名**：
/// * 带引号：路径到下一个引号为止；
/// * 不带引号（Word 的实际写法，路径里带空格）：参数一定以 ` /` 开头。
fn exe_name(cmd: &str) -> &str {
    let s = cmd.trim().trim_matches('"');
    let path = if cmd.trim_start().starts_with('"') {
        match s.find('"') {
            Some(i) => &s[..i],
            None => s,
        }
    } else {
        match s.find(" /") {
            Some(i) => &s[..i],
            None => s,
        }
    };
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

// ==================== 注册表（薄薄一层）====================

/// 读 `HKEY_CLASSES_ROOT\<progid>` 存不存在。存在 = 有程序认领这个 ProgID。
fn progid_registered(progid: &str) -> bool {
    unsafe { open_key(progid).is_some() }
}

/// `Word.Application` → CLSID → `LocalServer32`（默认值）。
fn word_local_server() -> Option<String> {
    unsafe {
        let k = open_key("Word.Application\\CLSID")?;
        let clsid = query_default(k);
        let _ = RegCloseKey(k);
        let clsid = clsid?;
        let k = open_key(&format!("CLSID\\{clsid}\\LocalServer32"))?;
        let srv = query_default(k);
        let _ = RegCloseKey(k);
        srv
    }
}

/// 打开 HKCR 下的子键。失败（不存在）返回 `None` —— 这是**正常结果**，不是错误。
unsafe fn open_key(sub: &str) -> Option<HKEY> {
    unsafe {
        let wide: Vec<u16> = sub.encode_utf16().chain(std::iter::once(0)).collect();
        let mut h = HKEY(std::ptr::null_mut());
        let rc = RegOpenKeyExW(HKEY_CLASSES_ROOT, PCWSTR(wide.as_ptr()), None, KEY_READ, &mut h);
        (rc.0 == 0).then_some(h)
    }
}

/// 读一个键的**默认值**（`REG_SZ`）。类型不对、读不出来都给 `None`。
unsafe fn query_default(h: HKEY) -> Option<String> {
    unsafe {
        let mut ty = REG_VALUE_TYPE(0);
        let mut cb: u32 = 0;
        // 两趟：先问字节数，再取数据（该 API 的约定用法）
        let rc = RegQueryValueExW(h, PCWSTR::null(), None, Some(&mut ty), None, Some(&mut cb));
        if rc.0 != 0 || cb == 0 {
            return None;
        }
        let mut buf = vec![0u8; cb as usize];
        let mut got = cb;
        let rc = RegQueryValueExW(
            h,
            PCWSTR::null(),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr()),
            Some(&mut got),
        );
        if rc.0 != 0 || ty != REG_SZ {
            return None;
        }
        let units: Vec<u16> = buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
        String::from_utf16(&units[..end]).ok()
    }
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;

    /// 正常机器：`Word.Application` 指向真正的 WINWORD.EXE。
    #[test]
    fn winword_means_word() {
        let srv = Some(r"C:\Program Files\Microsoft Office\Root\Office16\WINWORD.EXE /Automation");
        assert_eq!(classify(srv, false), EngineProbe::Word);
        // 同时装了 WPS 也不影响判断 —— 有 Word 就用 Word
        assert_eq!(classify(srv, true), EngineProbe::Word);
    }

    /// ★ 关键一条：WPS 把自己注册成 `Word.Application` 时，**不能**当成有 Word。
    #[test]
    fn wps_impersonating_word_is_not_word() {
        let srv = Some(r"C:\Users\x\AppData\Local\Kingsoft\WPS Office\12.1\office6\wps.exe");
        assert_eq!(classify(srv, true), EngineProbe::WpsOnly);
    }

    /// ProgID 指向别的程序、WPS 也没注册 → 两台都不是。
    #[test]
    fn foreign_progid_without_wps_is_none() {
        let srv = Some(r"C:\Other\wordpad.exe");
        assert_eq!(classify(srv, false), EngineProbe::None);
    }

    /// 只装 WPS：`Word.Application` 压根不存在。
    #[test]
    fn wps_only() {
        assert_eq!(classify(None, true), EngineProbe::WpsOnly);
        assert_eq!(classify(None, false), EngineProbe::None);
    }

    /// 命令行里带参数、路径带空格、末尾带引号，都要能抠出 `WINWORD.EXE`。
    #[test]
    fn exe_name_survives_quotes_and_args() {
        assert_eq!(
            exe_name(r#""C:\Program Files\Office16\WINWORD.EXE" /Automation"#),
            "WINWORD.EXE"
        );
        assert_eq!(
            exe_name(r"C:\Program Files\Microsoft Office\Root\Office16\WINWORD.EXE /Automation"),
            "WINWORD.EXE"
        );
        assert_eq!(exe_name("WINWORD.EXE"), "WINWORD.EXE");
    }

    /// 确认框正文必须带着实测数字 —— 防的正是有人把它改成"可能略有差异"这种
    /// 等于没说的措辞（那就不配让人拿它做判断）。
    #[test]
    fn confirm_body_states_the_measured_damage() {
        assert!(WPS_CONFIRM_BODY.contains("少 1 页"), "{WPS_CONFIRM_BODY}");
        assert!(WPS_CONFIRM_BODY.contains("of 22"), "{WPS_CONFIRM_BODY}");
        assert!(WPS_CONFIRM_BODY.contains("WPS"), "{WPS_CONFIRM_BODY}");
        assert!(WPS_CONFIRM_BODY.contains("Tagged"), "{WPS_CONFIRM_BODY}");
    }

    /// 界面提示一律要短。**用字符数钉住**：这类文案没人管就会长成说明书
    /// （一开始确认框写了 6 行、横幅写了 3 句，被要求精简过一次）。
    #[test]
    fn ui_hints_stay_short() {
        for (name, s) in [
            ("WPS_ONLY_BANNER", WPS_ONLY_BANNER),
            ("WPS_CONFIRM_BODY", WPS_CONFIRM_BODY),
            ("PRINT_NEEDS_WORD", PRINT_NEEDS_WORD),
            ("NO_ENGINE", NO_ENGINE),
        ] {
            let n = s.chars().count();
            assert!(n <= 70, "{name} 太长（{n} 字）：{s}");
            assert!(!s.contains('\n'), "{name} 不许换行分多段：{s}");
        }
    }
}
