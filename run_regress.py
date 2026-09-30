"""跑 regress.sh 并落盘日志。

踩过的坑（本机专有，别再花时间重踩）：

1. **不能用 `bash` 这个名字**。PATH 里 `C:\\WINDOWS\\system32\\bash.exe` 排在
   Git Bash 前面，那是 WSL 的入口存根，被本机沙箱的程序黑名单拦下，
   报错是 UTF-16LE 的「拒绝访问。」（按 UTF-8 看全是乱码，很容易误判成
   脚本语法错误）。一律用 Git Bash 的绝对路径。
2. 同理别用 shell 直接重定向 `> /d/test/xxx.log`：本会话里 `/d/` 挂载点
   时有时无，日志会根本没生成，还附赠一条 wsl.exe 被拦的提示。
   用 Python 起子进程、cwd 给 Windows 路径、日志自己写，稳定。
"""
import os
import subprocess
import sys

CWD = "D:/test/wrepl"
LOG = "D:/test/regress-m10.log"
BASH = "C:/Users/xinwei01307/scoop/apps/git/current/bin/bash.exe"


def main():
    if not os.path.isfile(BASH):
        print(f"找不到 Git Bash：{BASH}")
        return 1
    cmd = sys.argv[1:]
    log = LOG
    if "--log" in cmd:
        i = cmd.index("--log")
        log = cmd[i + 1]
        del cmd[i:i + 2]
    # 第一个参数若是 .sh 就当脚本名，否则整个都算是给 regress.sh 的位置参数
    # （regress.sh 用位置参数收「wrepl.exe 路径」「wrepl-gui.exe 路径」）。
    # 早先把 exe 路径直接摆在最前，bash 会把它当脚本去执行，报 126
    # "cannot execute binary file"。
    script = "regress.sh"
    if cmd and cmd[0].endswith(".sh"):
        script = cmd.pop(0)
    cmd = [BASH, script] + cmd

    r = subprocess.run(cmd, cwd=CWD, capture_output=True, text=True,
                       encoding="utf-8", errors="replace")
    with open(log, "w", encoding="utf-8") as f:
        f.write(r.stdout)
        if r.stderr.strip():
            f.write("\n──────── STDERR ────────\n")
            f.write(r.stderr)
    print(f"rc = {r.returncode}　日志 → {log}")
    if r.stderr.strip():
        print("STDERR:", r.stderr[:1500])
    tail = [l for l in r.stdout.splitlines() if l.strip()]
    print("\n".join(tail[-25:]))
    return r.returncode


if __name__ == "__main__":
    sys.exit(main())

