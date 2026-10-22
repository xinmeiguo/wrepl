//! egui_sauge 设计系统接入层。
//!
//! 迁移**第 2 步**：把设计系统的字体表、九档字号层级与主题接到 wrepl 上。
//! 组件替换只做「批量替换」一页（见 [`crate::app`]），但字体与主题是
//! **全程序**的 —— 这是设计系统的固有性质（`apply_theme_with` 写的是
//! `egui::Context` 级样式），也和第 2 步里「导航改成左侧 `NavItem` 边栏」
//! 这项本就属于全局的改动一致。
//!
//! ## 初始化顺序（**不能动**，踩过坑）
//!
//! 1. [`egui_sauge::install_fonts`] —— 它内部是
//!    `FontDefinitions::default() + Phosphor`，**无条件覆盖**整个字体表；
//!    同时写入九档字号层级（`Style::text_styles`）。
//!    README 说「把自定义字体注册在 `install_fonts` **之前**」，与实现**相反**：
//!    先注册中文字体会被这一步整个冲掉 → 中文全变豆腐块。
//! 2. 自己**从头**拼一份「egui 默认 + Phosphor + 中文」，再 `set_fonts` 覆盖第 1 步那份。
//!    最终 `Proportional` 的顺序是 `["cjk", "phosphor", …egui 默认…]` —— **中文在
//!    最前、Phosphor 紧随其后**，理由见下面 [`CJK_CANDIDATES`] 上方的长注释
//!    （Phosphor 的 cmap 覆盖 a–z 却不含 A–Z/数字，放前面会把小写字母吞成空白）。
//!    ★ 不能走「`ctx.fonts(|f| f.definitions().clone())` 取回来再追加」那条路：
//!      eframe 的创建闭包跑在 `Context::run()` **之前**，此时字体表还不存在，
//!      取用会 panic（`No fonts available until first call to Context::run()`，
//!      实测于 egui 0.34.3）。
//!      字号层级存在 `Style` 里，`set_fonts` 不会动它，所以第 1 步的成果不丢。
//! 3. [`apply`] —— 主题（palette + density + egui 自己的 theme 偏好）。

use eframe::egui;
use egui_sauge::{Density, Locale, Palette};

// 组件与令牌按需再导出，`app.rs` 只 `use crate::ds::…` 一处。
pub use egui_sauge::components::{
    Alert, Button, ButtonSize, Card, Checkbox, ConfirmDialog, InputField, Level, NavItem,
    ProgressBar, RadioGroup, RadioOption, SelectField,
};
pub use egui_sauge::{Icon, RADIUS, SPACING, palette_of};

/// 中文字体在字体表里的键名。
const CJK_KEY: &str = "cjk";
/// Phosphor 图标字体在字体表里的键名（`egui_phosphor::add_to_fonts` 用的就是它）。
const PHOSPHOR_KEY: &str = "phosphor";

/// 候选中文字体：**按「更现代」排，前面的优先**。
///
/// 只读系统已装的字体、**不打包进产物** —— 零体积增长、无字体授权问题，
/// 也解释了为什么产物一直是 6~7 MB（对比：嵌一个中文字体要 +5~10 MB）。
///
/// 前 8 项是更现代的中文无衬线（小米 MiSans / 华为 HarmonyOS Sans / 思源黑体 /
/// Noto Sans SC / 阿里普惠体 / OPPO Sans）。**本机一个都没装**，所以行为与迁移前
/// 完全一致；哪天装了，重启程序就自动用上，不用改代码。
/// 后面五项是 Windows 保底，尤其 `msyh.ttc`（微软雅黑）—— 简体中文系统的标配，
/// 字形覆盖最全，作为兜底最稳。
///
/// 注意 `.ttc` 是字体集合，靠 `FontData::index` 选第几张字面。
const CJK_CANDIDATES: &[(&str, u32)] = &[
    // ── 更现代的中文黑体（装了才生效，都是免费商用授权）──
    ("C:/Windows/Fonts/MiSans-Regular.ttf", 0),
    ("C:/Windows/Fonts/MiSans-Regular.otf", 0),
    ("C:/Windows/Fonts/HarmonyOS_Sans_SC_Regular.ttf", 0),
    ("C:/Windows/Fonts/SourceHanSansSC-Regular.otf", 0),
    ("C:/Windows/Fonts/SourceHanSansCN-Regular.otf", 0),
    ("C:/Windows/Fonts/NotoSansSC-Regular.otf", 0),
    ("C:/Windows/Fonts/AlibabaPuHuiTi-3-55-Regular.ttf", 0),
    ("C:/Windows/Fonts/OPPOSans-R.ttf", 0),
    // ── Windows 保底 ──
    ("C:/Windows/Fonts/msyh.ttc", 0),
    ("C:/Windows/Fonts/msyhbd.ttc", 0),
    ("C:/Windows/Fonts/simhei.ttf", 0),
    ("C:/Windows/Fonts/simsun.ttc", 0),
    ("C:/Windows/Fonts/Deng.ttf", 0),
];

/// 装字体与字号层级。**开机一次**（`run_native` 的创建闭包 / 无头自检开头）。
pub fn install(ctx: &egui::Context) {
    // ── 1. 设计系统自带那一步：Phosphor 图标字体 + 九档字号层级 ───────────
    egui_sauge::install_fonts(ctx);

    // ── 2. 自己拼一份含中文的字体表，覆盖掉第 1 步那份 ────────────────────
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

    let mut used = String::from("(系统默认)");
    for (path, idx) in CJK_CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fd = egui::FontData::from_owned(bytes);
        fd.index = *idx;
        fonts.font_data.insert(CJK_KEY.to_owned(), fd.into());

        // Proportional 的目标顺序：`["cjk", "phosphor", …egui 自带…]`
        //
        // ★ 这两个的先后**不能反**，踩过坑：
        //   `Phosphor.ttf` 的 cmap 里 **a–z 全 26 个小写字母都有映射**
        //   （实测，图标字体的连字层占用了这些码位），却**不含** A–Z、0–9 与汉字。
        //   若把 `phosphor` 排到中文字体**前面**，`Excel` 会被它逐字母接走 →
        //   渲染成空白，界面上就只剩 `E`（同批实测：`.bak`→`.`、`Word`→`W`、
        //   全小写的 `wrepl` 整个消失、路径 `E:/test/docx`→`E://`）。
        //   中文字体放最前，先把这些字符接走就没这问题。
        //   而图标字形在私用区（U+E000–F8FF），**雅黑的 cmap 里一个私用区码位都没有**
        //   （实测 0 个），所以图标照样落到 `phosphor` —— 两头都稳。
        //   · 拉丁与中文都落到中文字体 —— 与迁移前一致（迁移前 wrepl 就是把中文
        //     `insert(0)`，雅黑同时带拉丁与汉字），其余页面的观感因此不变；
        //   · emoji 与符号继续走 egui 自带那几款，排最后兜底。
        //
        // 注：`egui_phosphor::add_to_fonts` 是把 `phosphor` 插进 **Proportional 的
        // 第 1 位**（即 egui 默认字体**之后**，源码 lib.rs:10 `font_keys.insert(1, …)`），
        // 且**完全不动 Monospace**，所以这里两个家族都得自己摆。
        let prop = fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default();
        prop.retain(|k| k != CJK_KEY && k != PHOSPHOR_KEY);
        prop.insert(0, CJK_KEY.to_owned());
        prop.insert(1, PHOSPHOR_KEY.to_owned());

        // Monospace 只补一份中文兜底（日志里的中文），排末尾 —— 迁移前就是 push。
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push(CJK_KEY.to_owned());

        used = (*path).to_string();
        break;
    }
    ctx.set_fonts(fonts);

    // ── 3. 语言：只影响设计系统自己那几串（确认框默认按钮、StatusDot 文案）。
    //        wrepl 的界面文字全是自带的，且确认框都显式传了中文标签，
    //        这里固定 En 只为**确定性** —— 不跟随系统语言飘。 ─────────────
    egui_sauge::set_locale(ctx, Locale::En);

    // 把实际选中的字体写进日志 —— 以后有人问「为什么看着不一样」，
    // 这一行能直接回答（而不是靠猜他机器上装了什么）。
    crate::diag::log(format!("设计系统就绪：界面字体 {used}"));
}

/// 「⋯」—— 主按钮旁边的**旁路入口**（2026-10-22）。
///
/// 主按钮走系统原生对话框；这三个小点是给熟路的人留的快速通道（自绘面板）。
/// 刻意做得不显眼：默认 `text_tertiary`、无边框无背景，只有悬停时才亮起来、
/// 浮出一行说明 —— 它不该在第一眼就被当成一个操作按钮，否则又会有人问
/// "这个点点是什么"。
///
/// 边长写死 26（= `ButtonSize::Sm` 的高度；那个 `height()` 不对外）。它和左边
/// 那颗 Md 按钮在 `horizontal` 里按基线居中排布，不会把行高顶歪。
///
/// 返回"被点了没有"，调用方拿到 `true` 就去开自绘面板。
pub fn dots(ui: &mut egui::Ui, tip: &str) -> bool {
    let pal = palette_of(ui.ctx());
    let side = 26.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        if resp.hovered() {
            ui.painter().rect_filled(rect, RADIUS.md, pal.bg_hover);
        }
        let color = if resp.hovered() {
            pal.text_primary
        } else {
            pal.text_tertiary
        };
        let r = egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(13.0));
        Icon::DotsHorizontal.paint(ui.painter(), r, color);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(tip).clicked()
}

/// 应用设计系统主题。`light` = 浅色还是深色 palette。
///
/// **全局**（写 `Context` 级样式）：在 `render` 里按 `light_theme` 变化触发一次，
/// 位置与迁移前的 `ctx.set_visuals(…)` 完全一致。
///
/// 密度固定 [`Density::Comfortable`]：GUIDE §8 说 >50 行才切 `Compact`，
/// 而规则表只是页内一块（且已虚拟化），整页切 Compact 会把页眉与表单一起压扁。
pub fn apply(ctx: &egui::Context, light: bool) {
    // 让 egui 自己的 theme 偏好跟着走：`visuals.dark_mode`、内置控件（滚动条、
    // 文本选中）都读它。光调 `apply_theme_with` 不够 —— 它只写 style，不改主题偏好。
    ctx.set_theme(if light {
        egui::ThemePreference::Light
    } else {
        egui::ThemePreference::Dark
    });
    let palette = if light {
        Palette::light()
    } else {
        Palette::dark()
    };
    egui_sauge::apply_theme_with(ctx, &palette, Density::Comfortable);

    // 滚动条改成**常显的细条**（2026-10-11）。
    //
    // egui 默认的 `ScrollStyle::floating()` 把 `dormant_handle_opacity` 设成 0.0：
    // 条**画了**，但不滚动、不悬停到条上时透明度是 0 —— 也就是"其实有、看不见"。
    // 界面里会出内滚动条的那几处（替换规则表 / 运行日志 / 跳过清单）恰恰需要
    // "一眼看出内容被截断了"，所以按 egui 自己在 `ScrollStyle::floating_allocated_width`
    // 上的推荐取值改成常显：预留 4px、休眠期也给 0.45 的不透明度。
    //
    // · 只作用于"内容确实超出"的滚动区 —— 可见性仍是 `VisibleWhenNeeded`，
    //   装得下的时候不会平白多出一条；
    // · 走 `all_styles_mut` 是为了浅/深两套 Style 都改到（`set_theme` 会切另一套）；
    // · 放在 `apply_theme_with` **之后**：那一步会重写 style，写在前面会被盖掉。
    ctx.all_styles_mut(|style| {
        let sc = &mut style.spacing.scroll;
        sc.floating = true;
        sc.floating_width = 4.0;
        sc.floating_allocated_width = 4.0;
        sc.dormant_handle_opacity = 0.45;
        sc.dormant_background_opacity = 0.0;
        sc.active_handle_opacity = 0.65;
        sc.interact_handle_opacity = 0.9;
    });
}

/// 窗口清屏色，给 `eframe::App::clear_color` 用（返回的是**线性化前**的 0–1 分量）。
///
/// ★ 为什么必须显式给（2026-10-11，追"中间那条黑条"追出来的）
///
/// eframe 的默认实现返回**全透明**，落到窗口上就是**纯黑**。平时看不出来 ——
/// `Panel::left` / `CentralPanel` 各自把自己的矩形铺满；但只要两块面板之间留下一道
/// 谁都没刷到的缝，缝里露出的就是这个清屏色。
///
/// 实测（给 `panel_fill` 涂蓝、`noninteractive.bg_stroke` 涂品红的色标探针）：
/// 侧栏右侧那条 11px 的"黑条"换遍所有主题色纹丝不动，而真正的 1px 分隔线在它右边
/// 一格 —— 说明它压根不是画出来的，是 `Panel::left`（egui 0.34 前的
/// `SidePanel`）的 `Frame` 只铺到 `exact_size`
/// 宽、而面板预留宽度还多一圈内边距，余下那一段没人画。
///
/// 设成主题的 `bg_app` 之后，这类缝即使还在也与两侧同色（等于隐身）。
/// 这只是**兜底**：真正消缝的是 `app.rs` 里 `Panel::left` 那句
/// `ui.set_min_width(ui.available_width())`。
pub fn window_bg(light: bool) -> [f32; 4] {
    let c = if light {
        Palette::light()
    } else {
        Palette::dark()
    }
    .bg_app;
    let [r, g, b, _] = c.to_array();
    [
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        1.0,
    ]
}
