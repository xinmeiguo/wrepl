//! Word COM 枚举常量（与 C# 版 Core/Wd.cs 一一对应，数值来自 Word 对象模型）。
//!
//! 2026-10-19：页眉 COM 引擎退役后，只服务于它的一组常量（页眉部件号、
//! 下划线 / 行高 / 列宽 / 段落对齐 / 单元格垂直对齐）一并删除 —— 留着只会
//! 以 `never used` 的形式把编译输出弄脏。

pub const ALERTS_NONE: i32 = 0; // wdAlertsNone
pub const PRINT_RANGE_OF_PAGES: i32 = 4; // wdPrintRangeOfPages
pub const DO_NOT_SAVE_CHANGES: i32 = 0; // wdDoNotSaveChanges
pub const AUTOMATION_SECURITY_FORCE_DISABLE: i32 = 3; // msoAutomationSecurityForceDisable
pub const EXPORT_FORMAT_PDF: i32 = 17; // wdExportFormatPDF
pub const EXPORT_OPTIMIZE_FOR_PRINT: i32 = 0; // wdExportOptimizeForPrint
pub const EXPORT_RANGE_FROM_TO: i32 = 3; // wdExportFromTo
