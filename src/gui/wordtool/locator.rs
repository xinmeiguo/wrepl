//! 定位「页眉第一个表格的 (1,1) 单元格」（等价 C# Core/HeaderCellLocator.cs）。
//! 与 C# 版逐行为等价；中间 COM 对象（section/header/range/tables/rows/columns）
//! 均在当次迭代结束即 Release，无 RCW 累积。

use super::com::{Disp, DispResult, res_disp, res_i32, var_i4};
use super::wd;

/// 对文档每个节的首页眉第一个表格的 (1,1) 单元格执行操作；无表格的节自动跳过。
pub unsafe fn for_each_first_cell<F>(doc: &Disp, mut action: F) -> DispResult<()>
where
    F: FnMut(&Disp, &Disp) -> DispResult<()>,
{
    unsafe {
        let sections = res_disp(doc.get("Sections")?)?;
        let count = res_i32(sections.get("Count")?)?;
        for i in 1..=count {
            let section = res_disp(sections.call("Item", vec![var_i4(i)])?)?;
            let headers = res_disp(section.get("Headers")?)?;
            let header = res_disp(headers.call("Item", vec![var_i4(wd::HEADER_FOOTER_PRIMARY)])?)?;
            let range = res_disp(header.get("Range")?)?;
            let tables = res_disp(range.get("Tables")?)?;
            if res_i32(tables.get("Count")?)? == 0 {
                continue;
            }
            let tbl = res_disp(tables.call("Item", vec![var_i4(1)])?)?;
            let rows = res_disp(tbl.get("Rows")?)?;
            let cols = res_disp(tbl.get("Columns")?)?;
            if res_i32(rows.get("Count")?)? < 1 || res_i32(cols.get("Count")?)? < 1 {
                continue;
            }
            let cell = res_disp(tbl.call("Cell", vec![var_i4(1), var_i4(1)])?)?;
            action(&cell, &tbl)?;
        }
        Ok(())
    }
}
