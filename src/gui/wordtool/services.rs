//! 三个业务服务（等价 C# Services/ 下的 TextReplaceService / ImageReplaceService / PrintService）。

use super::com::{Disp, DispResult, res_disp, res_f32, var_bool, var_bstr, var_i4, var_r4};
use super::locator::for_each_first_cell;
use super::wd;
use super::word_session::{run_on_documents, run_on_documents_printing};

// ==================== 文本替换 ====================

const EAST_ASIA_FONT: &str = "宋体";
const LATIN_FONT: &str = "Times New Roman";
const FONT_SIZE: f32 = 9.0; // 小五
const MANUAL_LINE_BREAK: char = '\u{000B}'; // Word 手动换行符 Chr(11)

/// 文本替换：清空首单元格，写入「英文(上) + 手动换行符 + 中文(下)」；仅中文时纯中文。
pub fn text_replace_run(
    doc_paths: &[String],
    english: &str,
    chinese: &str,
    log: &mut dyn FnMut(String),
    progress: &mut dyn FnMut(usize, usize),
) -> (i32, i32) {
    let insert_text = compose(english, chinese);
    run_on_documents(
        doc_paths,
        |doc| unsafe { for_each_first_cell(doc, |cell, _tbl| replace_in_cell(cell, &insert_text)) },
        log,
        progress,
        true,
        "已完成",
    )
}

/// 「英文 + 手动换行 + 中文」；仅中文则纯中文（等价 C# Compose）。
fn compose(english: &str, chinese: &str) -> String {
    let en = english.trim();
    let cn = chinese.trim();
    if !en.is_empty() && !cn.is_empty() {
        return format!("{en}{MANUAL_LINE_BREAK}{cn}");
    }
    if !cn.is_empty() {
        cn.to_string()
    } else {
        en.to_string()
    }
}

unsafe fn replace_in_cell(cell: &Disp, insert_text: &str) -> DispResult<()> {
    unsafe {
        {
            let range = res_disp(cell.get("Range")?)?;
            range.put("Text", var_bstr(""))?;
        }
        {
            let range = res_disp(cell.get("Range")?)?;
            range.put("Text", var_bstr(insert_text))?;
        }
        // 中英文按字符脚本自动分流：东亚字符用宋体，ASCII 用 Times New Roman
        let range = res_disp(cell.get("Range")?)?;
        let font = res_disp(range.get("Font")?)?;
        font.put("NameFarEast", var_bstr(EAST_ASIA_FONT))?;
        font.put("Name", var_bstr(LATIN_FONT))?;
        font.put("Size", var_r4(FONT_SIZE))?;
        font.put("Bold", var_i4(0))?;
        font.put("Italic", var_i4(0))?;
        font.put("Underline", var_i4(wd::UNDERLINE_NONE))?;
        Ok(())
    }
}

// ==================== 图片替换 ====================

/// 安全边距系数：图片只占用单元格可用区域的 90%，给上下/左右边框留余量。
const MARGIN_RATIO: f32 = 0.90;

/// 图片替换：把每个文档「首页眉」第一个表格的 (1,1) 单元格清空并插入图片。
/// scale_to_table=true：按表格高度等比缩放（含安全边距），不撑宽表格；
/// scale_to_table=false：保持图片原始尺寸，表格自适应图片大小。
pub fn image_replace_run(
    doc_paths: &[String],
    img_path: &str,
    scale_to_table: bool,
    log: &mut dyn FnMut(String),
    progress: &mut dyn FnMut(usize, usize),
) -> (i32, i32) {
    run_on_documents(
        doc_paths,
        |doc| unsafe {
            for_each_first_cell(doc, |cell, tbl| {
                replace_in_cell_img(cell, tbl, img_path, scale_to_table)
            })
        },
        log,
        progress,
        true,
        "已完成",
    )
}

unsafe fn replace_in_cell_img(
    cell: &Disp,
    tbl: &Disp,
    img_path: &str,
    scale_to_table: bool,
) -> DispResult<()> {
    unsafe {
        {
            let range = res_disp(cell.get("Range")?)?;
            range.put("Text", var_bstr(""))?; // 清空（同时清掉旧图片/文字）
        }

        // 关键：必须在插入图片【之前】测量单元格尺寸！
        // 表格开启自适应时，插入大图会先把单元格撑大，
        // 插入后再量到的已是撑大后的错误尺寸，缩放与锁列将全部失效。
        let (mut cw, mut rh) = (0.0f32, 0.0f32);
        if scale_to_table {
            cw = res_f32(cell.get("Width")?)?; // 首列原始宽度
            let rows = res_disp(tbl.get("Rows")?)?;
            let row1 = res_disp(rows.call("Item", vec![var_i4(1)])?)?;
            rh = res_f32(row1.get("Height")?)?; // 首行高度；自动行高时返回 0
        }

        let range = res_disp(cell.get("Range")?)?;
        let shapes = res_disp(range.get("InlineShapes")?)?;
        let pic = res_disp(shapes.call(
            "AddPicture",
            vec![var_bstr(img_path), var_bool(false), var_bool(true)],
        )?)?;

        if scale_to_table {
            scale_to_cell(tbl, &pic, cw, rh)?;
        }
        // 否则保持原尺寸，表格自适应图片

        center_in_cell(cell);
        Ok(())
    }
}

/// contain 适配：先按行高缩放，宽度超限则改按列宽，整体乘安全边距系数（四边各留 5%）。
unsafe fn scale_to_cell(tbl: &Disp, pic: &Disp, cw: f32, rh: f32) -> DispResult<()> {
    unsafe {
        let ow = res_f32(pic.get("Width")?)?;
        let oh = res_f32(pic.get("Height")?)?;

        let avail_h = if rh > 1.0 { rh } else { oh }; // 自动行高时以图片原高兜底，后面再补行高
        let avail_w = if cw > 1.0 { cw } else { ow };

        let mut scale = avail_h * MARGIN_RATIO / oh;
        if ow * scale > avail_w * MARGIN_RATIO {
            scale = avail_w * MARGIN_RATIO / ow;
        }
        if !(scale > 0.0) {
            scale = 1.0; // 防御非法比例
        }

        pic.put("Width", var_r4(ow * scale))?;
        pic.put("Height", var_r4(oh * scale))?;

        fix_row_height_if_auto(tbl, rh, oh * scale);
        lock_first_column(tbl, cw);
        Ok(())
    }
}

/// 表格为自动行高时，手动把行高设为图片高度 + 余量，避免图片被截断。
unsafe fn fix_row_height_if_auto(tbl: &Disp, rh: f32, final_pic_height: f32) {
    if rh > 0.0 {
        return;
    }
    let _ = unsafe {
        (|| -> DispResult<()> {
            let rows = res_disp(tbl.get("Rows")?)?;
            let row = res_disp(rows.call("Item", vec![var_i4(1)])?)?;
            row.put("Height", var_r4(final_pic_height + 2.0))?; // +2pt 余量
            row.put("HeightRule", var_i4(wd::ROW_HEIGHT_EXACTLY))?;
            Ok(())
        })()
    };
}

/// 锁定首列首选宽度并关闭自动调整，确保表格不会被图片撑大。
unsafe fn lock_first_column(tbl: &Disp, column_width: f32) {
    let _ = unsafe {
        (|| -> DispResult<()> {
            tbl.put("AllowAutoFit", var_bool(false))?;
            let cols = res_disp(tbl.get("Columns")?)?;
            let col = res_disp(cols.call("Item", vec![var_i4(1)])?)?;
            col.put("PreferredWidth", var_r4(column_width))?;
            col.put("PreferredWidthType", var_i4(wd::PREFERRED_WIDTH_POINTS))?;
            Ok(())
        })()
    };
}

/// 图片在单元格内居中：水平（段落居中）+ 垂直（单元格垂直居中），并清段落间距防止顶边框。
unsafe fn center_in_cell(cell: &Disp) {
    let _ = unsafe {
        (|| -> DispResult<()> {
            let range = res_disp(cell.get("Range")?)?;
            let pf = res_disp(range.get("ParagraphFormat")?)?;
            pf.put("Alignment", var_i4(wd::ALIGN_PARAGRAPH_CENTER))?;
            cell.put("VerticalAlignment", var_i4(wd::CELL_ALIGN_VERTICAL_CENTER))?;
            pf.put("SpaceBefore", var_r4(0.0))?;
            pf.put("SpaceAfter", var_r4(0.0))?;
            Ok(())
        })()
    };
}

// ==================== 批量打印 ====================

/// 批量打印：page_range 留空打印全部，否则按 "1-3,5,7-9" 格式打印指定页。
///
/// `printer` 是 Word 的 `ActivePrinter` 字符串（`"名称 on 端口"`，见
/// [`super::printers`]）；`None` = 用系统默认那台。
pub fn print_run(
    doc_paths: &[String],
    page_range: &str,
    printer: Option<&str>,
    log: &mut dyn FnMut(String),
    progress: &mut dyn FnMut(usize, usize),
) -> (i32, i32) {
    let pages = page_range.trim().to_string();
    run_on_documents_printing(
        doc_paths,
        |doc| unsafe {
            // Background=false 同步打印，避免 Close 抢在打印完成前关闭文档
            if pages.is_empty() {
                doc.call_named("PrintOut", vec![("Background", var_bool(false))])?;
            } else {
                doc.call_named(
                    "PrintOut",
                    vec![
                        ("Background", var_bool(false)),
                        ("Range", var_i4(wd::PRINT_RANGE_OF_PAGES)),
                        ("Pages", var_bstr(&pages)),
                    ],
                )?;
            }
            Ok(())
        },
        log,
        progress,
        false, // 打印不改动文档，无需保存
        "已发送打印",
        printer,
    )
}
