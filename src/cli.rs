//! 命令行界面定义。
//!
//! 这里**只有参数**，没有逻辑。GUI 不依赖本模块（它自己收参数），
//! 但两边最终调用的都是 `wrepl` 库里同一套实现。

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "wrepl",
    version,
    about = "Word 批量替换工具（.docx 批量查找替换，原有格式保持不变）",
    long_about = "按规则批量替换 .docx 中的文本。未命中的内容原样保留，\
                  不重新排版、不重建文档；执行后可选自动验证格式未被动过。\n\
                  \n\
                  想用图形界面？双击同一文件夹里的 wrepl-gui.exe —— 与本工具共用同一套内核。"
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// 目标文件 / 目录的选择参数。
#[derive(Args, Debug, Clone)]
pub struct Targets {
    /// 目标：.docx 文件，或包含 .docx 的目录（可给多个）
    #[arg(value_name = "PATH", required = true)]
    pub paths: Vec<PathBuf>,

    /// 目录递归遍历
    #[arg(long)]
    pub recursive: bool,

    /// 额外排除的通配符（可重复），如 --exclude "*_bak*"
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// 并行处理的线程数：默认 0＝按本机可用并行度自动；给 1 就是强制串行。
    /// 同一批文件无论几线程，产物与报告都逐字节相同，这里只是快慢之别。
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub threads: usize,
}

/// 规则来源与默认选项。
#[derive(Args, Debug, Clone)]
pub struct RuleSource {
    /// 逐条给规则：--rule "查找=>替换"（可重复）
    #[arg(long = "rule", value_name = "FIND=>REPLACE")]
    pub rule: Vec<String>,

    /// 规则文件：每行一条
    /// （`查找<TAB>替换[<TAB>作用域[<TAB>选项[<TAB>备注]]]`，或 `查找=>替换`；# 开头为注释）
    #[arg(long = "rules-file", value_name = "FILE")]
    pub rules_file: Option<PathBuf>,

    /// Excel 规则表：按工作表顺序取**第一张读得出条款的表**
    /// （表名随意，不必叫「规则」；第 1 列＝查找内容，第 2 列＝替换为，其余列按列名认，列序随意）
    #[arg(long = "rules-book", value_name = "XLSX")]
    pub rules_book: Option<PathBuf>,

    /// 所有规则的默认作用域：正文,页眉页脚,文本框,脚注,批注,文件名（或 全部）
    #[arg(long, default_value = "全部")]
    pub scope: String,

    /// 所有规则默认区分大小写
    #[arg(long)]
    pub case_sensitive: bool,

    /// 所有规则默认全字匹配（注意：不是正则 \b，定义见文档）
    #[arg(long)]
    pub whole_word: bool,

    /// 所有规则默认区分全角/半角
    #[arg(long)]
    pub kana_sensitive: bool,

    /// 链式执行：前一条规则的输出作为后一条的输入（默认各规则独立基于原文）
    #[arg(long)]
    pub chain: bool,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// 列出 docx 内部所有 part 及其指纹（摸底用）
    Inspect {
        /// 目标 docx
        file: PathBuf,
    },

    /// 零改动透传：读入再写出，验证拷贝过程不改变任何 part 内容
    Passthrough {
        /// 输入 docx
        input: PathBuf,
        /// 输出 docx
        output: PathBuf,
    },

    /// 比对两个 docx 的 part 级差异
    Diff {
        /// 左侧 docx
        a: PathBuf,
        /// 右侧 docx
        b: PathBuf,
    },

    /// 导出「段落 → run 切分」报告（人工核对偏移是否正确）
    Dump {
        /// 目标 docx
        file: PathBuf,
        /// 只处理指定 part（如 word/header1.xml）；默认全部文本容器
        #[arg(long)]
        part: Option<String>,
        /// 只显示可见文本包含该子串的段落
        #[arg(long)]
        grep: Option<String>,
        /// 每个 part 最多显示多少段落（0 = 不限）
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// 是否展示空元素 / 修订删除 / 域代码等特殊节点
        #[arg(long, default_value_t = true)]
        special: bool,
    },

    /// 侦察：扫目录，列出里面出现的候选项目编号与客户名
    Probe {
        /// 目标目录
        dir: PathBuf,
        /// 递归子目录
        #[arg(long)]
        recursive: bool,
        /// 把建议规则写成文本规则文件（查找列为候选串，替换列留空待填）
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },

    /// 预览：只扫描不落盘，列出每条规则的命中位置与条数
    Scan {
        #[command(flatten)]
        targets: Targets,
        #[command(flatten)]
        rules: RuleSource,
        /// 规则命中区间重叠时，只让「查找内容更长」的那条生效（默认两条都不改并报冲突）。
        /// 预演必须与 apply 用同一套裁决，否则"预览两条都不改、执行却改了"——
        /// 预览就失去意义了
        #[arg(long)]
        longest_first: bool,
    },

    /// 执行替换并落盘（默认就地替换源文件；用 --out 输出到副本目录）
    Apply {
        #[command(flatten)]
        targets: Targets,
        #[command(flatten)]
        rules: RuleSource,
        /// 输出目录：产物写到新目录，源文件不动（不给就就地替换源文件）
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// 就地替换源文件（不给 --out 时的默认行为；在原文件上直接覆盖改写）
        #[arg(long, conflicts_with = "out")]
        in_place: bool,
        /// 就地替换时保留一份 .docx.bak 备份（默认不留；只首次生成）。
        /// 只对就地替换有意义——写副本时源文件本来就不动
        #[arg(long, conflicts_with = "out")]
        backup: bool,
        /// 导出结果报告到 .xlsx（里程碑 M6）
        #[arg(long, value_name = "XLSX")]
        report: Option<PathBuf>,
        /// 跑完整执行路径但不落盘（等价于 scan，用于验证规则）
        #[arg(long)]
        dry_run: bool,
        /// 同步替换文件名：同一套规则也作用到文件名上（默认关闭）
        #[arg(long)]
        rename_files: bool,
        /// 执行后自动验证：残留自检 + 格式保全（关卡 1+2）
        #[arg(long)]
        verify_after: bool,
        /// 规则命中区间重叠时，只让「查找内容更长」的那条生效（默认两条都不改并报冲突）
        #[arg(long)]
        longest_first: bool,
        /// 完整镜像：未命中的文件也原样复制到输出目录，文件名一并归一
        /// （默认只把改过的文件写进输出目录）
        #[arg(long, requires = "out")]
        mirror: bool,
    },

    /// 规则管理：template（生成模板）/ dump（把规则导出成文本）/ check（只校验不执行）
    Rules {
        #[command(subcommand)]
        sub: RulesCmd,
    },

    /// 验证关卡 1 + 2：part 字节比对 + XML 语义骨架比对
    Verify {
        /// 处理前的 docx（--batch 时是原目录）
        a: PathBuf,
        /// 处理后的 docx（--batch 时是产物目录）
        b: PathBuf,
        /// 只检查关卡 1
        #[arg(long)]
        level1_only: bool,
        /// 批量模式：A、B 是两个目录，逐个成对验证；
        /// 同名优先配对，改过名的按 XML 结构指纹配（只在唯一匹配时才配，歧义会列出来）
        #[arg(long)]
        batch: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum RulesCmd {
    /// 生成规则文件模板
    Template {
        /// 写到这个路径：.xlsx 生成规则表，其它扩展名写文本规则文件；省略则打印到屏幕
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// 生成的模板里带上这几条示例规则
        #[arg(long = "rule", value_name = "FIND=>REPLACE")]
        rule: Vec<String>,
    },
    /// 把规则（命令行 / 规则文件）导出成标准文本规则文件
    Dump {
        #[command(flatten)]
        rules: RuleSource,
        /// 写到这个路径；省略则打印到屏幕
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
    /// 只校验规则能否正确解析，不碰任何文件
    Check {
        #[command(flatten)]
        rules: RuleSource,
    },
}
