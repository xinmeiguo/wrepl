//! 枚举本机可用打印机（Win32 `EnumPrintersW`，level 2）。
//!
//! ## 为什么要从系统拿，而不是问 Word
//!
//! Word 的 COM 接口**没有**打印机列表 —— `Application.ActivePrinter` 只有
//! 「当前那一台」这一个属性（Excel 才有 `Dialogs` 那条路能列出全部）。
//! 所以：**列表从系统拿，切换仍交给 Word**（`ActivePrinter = "名称 on 端口"`）。
//!
//! ## 为什么要端口
//!
//! Word 的 `ActivePrinter` 收的是一根**带端口后缀的字符串**，形如
//! `"HP LaserJet M404 on Ne02:"`，而不是光一个打印机名。给不出端口，Word 就
//! 拒收（弹 COM 错误）。端口正好就在 `PRINTER_INFO_2W.pPortName` 里，
//! 取它拼出来即是 Word 要的形状 —— 这是本模块存在的全部理由。
//!
//! ## 什么时候调
//!
//! `EnumPrintersW` 是同步 API，装机上一般几毫秒。**只在进入打印页时做一次**
//! 并缓存住（外加重扫按钮），不放到绘制路径上每帧跑。

use windows::core::PCWSTR;
use windows::Win32::Graphics::Printing::{
    EnumPrintersW, GetDefaultPrinterW, PRINTER_ENUM_CONNECTIONS, PRINTER_ENUM_LOCAL,
    PRINTER_INFO_2W,
};

/// 一台打印机。
#[derive(Clone, Debug)]
pub struct Printer {
    /// 打印机名（网络打印机是 `\\服务器\共享名` 这种 UNC 形式）。
    pub name: String,
    /// 端口（`Ne02:` / `USB001` / `PORTPROMPT:` …）。可能为空。
    pub port: String,
    /// 是不是系统默认那台。
    pub is_default: bool,
}

impl Printer {
    /// Word 的 `Application.ActivePrinter` 要的字符串：`名称 on 端口`。
    ///
    /// 端口拿不到时**只给名字** —— 有些驱动确实不上报端口，
    /// 给名字至少还有机会被 Word 认下；硬拼一个空端口反而一定报错。
    pub fn active_string(&self) -> String {
        if self.port.is_empty() {
            self.name.clone()
        } else {
            format!("{} on {}", self.name, self.port)
        }
    }
}

/// 列出本机打印机（本地 + 已连接的网络打印机）。
///
/// 返回**按名字排序**的列表；失败时返回原因（调用方把它显示在打印页上，
/// 而不是让下拉框空着不说为什么）。
pub fn list() -> Result<Vec<Printer>, String> {
    let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;

    // ① 第一次调用只为拿「需要多少字节」；缓冲给空，必然以
    //    ERROR_INSUFFICIENT_BUFFER 失败 —— 这是该 API 的约定用法，不是错。
    let mut needed = 0u32;
    let mut returned = 0u32;
    unsafe {
        let _ = EnumPrintersW(flags, PCWSTR::null(), 2, None, &mut needed, &mut returned);
    }
    if needed == 0 {
        // 一台都没有：正常结果，不是错误。
        return Ok(Vec::new());
    }

    // ② `Vec<u64>` 而不是 `Vec<u8>`：`PRINTER_INFO_2W` 的对齐要求是 8 字节，
    //    用 `Vec<u8>`（对齐 1）拿到指针再转过去是未定义行为。
    let words = (needed as usize).div_ceil(8);
    let mut buf = vec![0u64; words];
    {
        let bytes: &mut [u8] = unsafe {
            std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), needed as usize)
        };
        unsafe {
            EnumPrintersW(
                flags,
                PCWSTR::null(),
                2,
                Some(bytes),
                &mut needed,
                &mut returned,
            )
            .map_err(|e| format!("EnumPrinters 失败：{e}"))?;
        }
        // `bytes` 借用到此结束 —— 下面要按 `PRINTER_INFO_2W` 读**同一块**内存，
        // 那时必须已经没有别的借用了。`needed` 只是容量，`returned` 才是条数。
    }

    // ③ 缓冲里的结构体是一段**紧密排列**的 `PRINTER_INFO_2W` 数组（长度为 `returned`）。
    let infos: &[PRINTER_INFO_2W] = unsafe {
        std::slice::from_raw_parts(buf.as_ptr().cast::<PRINTER_INFO_2W>(), returned as usize)
    };

    let default_name = default_printer();
    let mut out: Vec<Printer> = infos
        .iter()
        .map(|i| {
            let name = pwstr(i.pPrinterName);
            let port = pwstr(i.pPortName);
            let is_default = default_name
                .as_deref()
                .is_some_and(|d| d.eq_ignore_ascii_case(&name));
            Printer {
                name,
                port,
                is_default,
            }
        })
        .filter(|p| !p.name.is_empty())
        .collect();

    // 名字排序。**不把默认那台顶到最前** —— 默认项在下拉框里是单独一条
    // 「（用默认打印机）」，列表按名字排更好找。
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out.dedup_by(|a, b| a.name.eq_ignore_ascii_case(&b.name));
    Ok(out)
}

/// 系统默认打印机的名字。取不到就 `None`。
fn default_printer() -> Option<String> {
    let mut len = 0u32;
    unsafe {
        // 同样两趟：先问长度（这次会返回 FALSE 且 len 被填好）
        let _ = GetDefaultPrinterW(None, &mut len);
    }
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    let ok = unsafe { GetDefaultPrinterW(Some(windows::core::PWSTR(buf.as_mut_ptr())), &mut len) };
    if !ok.as_bool() {
        return None;
    }
    // `len` 含结尾的 NUL，砍掉再转。
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16(&buf[..end]).ok()
}

/// `PWSTR` → `String`。空指针给空串（而不是 panic）。
fn pwstr(p: windows::core::PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { p.to_string() }.unwrap_or_default()
}
