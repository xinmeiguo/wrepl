# bench —— 性能测量与"改没改坏"的对照工具

这些脚本不参与构建、不进发布产物，只在**本机**跑。
它们回答两个不同的问题：

| 问题 | 用哪个 |
|---|---|
| **改没改坏？**（产物字节 / stdout 是否与改动前一致） | `compare.ps1` |
| **改快了多少？**（端到端 A/B） | `final.ps1` |
| **慢在哪一步？**（不起进程，直接调库分步计时） | `../examples/profile.rs` |

## 前置条件

- 真实语料放在 `E:\test\docx`（26 个交付包）。
  想换目录就改各脚本顶部的 `$corpus`。
- `bench\small\`（<1 MB 的那批样本）会在缺失时**自动从语料里复制**。
- Python 解释器路径写在各脚本顶部（`$py`），换成你自己的即可。

## compare.ps1 —— 逐字节对照

需要一份**改动前**的二进制：`bench\baseline\wrepl-baseline.exe`。做法：

```bash
git archive HEAD | tar -x -C /tmp/wrepl-head     # 或任意目录
cd /tmp/wrepl-head && cargo build --release
cp target/release/wrepl.exe <本仓库>/bench/baseline/wrepl-baseline.exe
```

然后：

```powershell
pwsh -NoProfile -File bench\compare.ps1
```

它把新旧两个 exe 跑同一套参数，逐文件比 SHA256：

- **产物**：写副本 / 完整镜像＋改名 / 链式 / 最长匹配优先 / 单线程 /
  预演 / 4 种就地替换（含 `.bak`）；
- **stdout**：`scan` / `dump` / `inspect` / `probe` / `rules dump` /
  `rules check` / `apply --dry-run` / `verify --batch` / `verify` 成对。

报告 `.xlsx` 里有「报告生成时间」和「wrepl v\<版本\>」，天然每次都不同，
所以它单独交给 `xlsx_diff.py` 逐 sheet 逐单元格比（这两处抹掉）。

> 两次运行**用同一个输出路径**（跑完再改名搬走），这样报告里记的
> 「输入 / 输出」路径也完全一致 —— 否则光凭路径不同就会报一堆假失败。
> 就地替换类用例一律加 `--threads 1`：并行下完成顺序本来就不定，
> 拿它比 stdout 只会自找麻烦。

## final.ps1 —— 端到端 A/B

交错跑基线 / 新版（各取 6 次最小值），并先测出 `--version` 的耗时当作
**进程启动本底**扣掉 —— 本机的进程创建开销（企业管控记账）比程序真正干的活还大，
不扣掉会严重低估加速比。

```powershell
pwsh -NoProfile -File bench\final.ps1
```

其余几个脚本是过程中的分项测量，留作复现用：

| 脚本 | 用途 |
|---|---|
| `bench.ps1` | 各种子命令的最粗一轮耗时 |
| `e2e.ps1` | 按语料分组（全部 / 大文件 / 小文件）的端到端 A/B |
| `inplace.ps1` | 就地替换专项 A/B |
| `ab.ps1` | 把 `profile.exe` 的输出汇总成「步骤 × 基线 / 新版 / 加速」表 |
| `rules.txt` | 上面这些脚本共用的规则表 |
| `pe_resources.py` | 列 PE 的资源类型。**用来证明产物没有 manifest / 没有版本信息 / 没有图标** —— 详见 [../PERFORMANCE.md](../PERFORMANCE.md) 第 5 节 |
| **`win_probe.py`** | ★ **界面版到底有没有把窗口建出来。** 枚举该进程的可见顶层窗口（排除 winit 的内部辅助窗口），够大才算数。**这是唯一能自动回答这个问题的工具** —— 静态检查只看 PE 字段、`--selftest` 按设计不开窗，而"建不出窗口时进程同样是活的"，看日志也分不开 |

```powershell
# 界面版发版前必跑：40 s 内能不能出现主窗口（应 2~7 s）
python bench\win_probe.py target\release\wrepl-gui.exe 40
```

退出码：`0` 出现主窗口 ／ `3` 超时仍无窗口 ／ `4` 进程提前退出 ／ `5` 弹出了模态错误框。
后三种都要当**失败**处理。

> 产物的依赖、架构、子系统、主线程栈这四项，用仓库里那个 CI 脚本看：
> `python .github/scripts/check_pe_imports.py <exe>`。
> ★ 其中"主线程栈"这一项的判据是**不许被调大**（上限 4 MB），方向与直觉相反 ——
> 见 [../PERFORMANCE.md](../PERFORMANCE.md) 第 5.3.1 节的单变量实验。

## 两个本机专属的坑

1. **不能用 `std::env::temp_dir()` 落测试临时文件**：本机 `%TEMP%` 在 C 盘，
   而 C 盘禁止非系统进程写入 —— 沙箱里有时放行、脱离沙箱跑回归时又被拒，
   同一份代码会随"怎么跑的"红绿不定。单元测试现在写进 `target/<profile>/` 下。
2. **`wrepl-gui.exe` 是图形子系统进程**：用 `> 文件` 直接重定向时 PowerShell
   **不会等它**，`$LASTEXITCODE` 取不到；走管道（`| Out-String`）才会等。


## 测量上的两个坑

1. **单次测量不可信。** 本机 `write_at` 会随机撞上杀软独占刚落盘的文件 →
   落到退避重试，同一段代码能测出 23 ms 和 59 ms 两个数。一律**多次取最小**。
2. **别只看绝对耗时。** `--version` 在本机要 63–93 ms，其中属于程序的只有亚毫秒级。
   看**净耗时比**更接近真实观感。
