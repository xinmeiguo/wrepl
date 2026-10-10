# -*- coding: utf-8 -*-
"""判定 GUI 到底"窗口建出来了没有" —— 不靠日志、不靠感觉，直接枚举该进程的可见顶层窗口。

为什么需要它
------------
`diag_run.py` 只能看"进程活没活"，而**窗口建不出来时进程也活着**：
`eframe::run_native` 卡在建窗里（或弹了一个模态框），日志同样只停在「开始建窗」。
两者从日志上分不开，必须问 Windows 本身。

做法
----
EnumWindows 遍历顶层窗口 → GetWindowThreadProcessId 过滤出目标 pid
→ IsWindowVisible + GetWindowRect（面积>0）判定"真的有个窗口在屏幕上"。

用法
----
    python win_probe.py <exe> [观察秒数] [环境变量="K=V;K2=V2"]

退出码：0 = 出现可见窗口；3 = 超时仍无窗口；4 = 进程已退出
"""
import ctypes
import ctypes.wintypes as wt
import os
import subprocess
import sys
import time

u32 = ctypes.windll.user32

WNDENUMPROC = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)

# winit / egui 自己开的一堆**内部辅助窗口**，面积小、无标题，别拿它们当"窗口出来了"。
# 实测对照组（能用的 v0.2.2）在 2 s 时就会出现 `Winit Thread Event Target`（16x16）——
# 只按"可见 + 有面积"筛，会把**建不出主窗口**的情况也判成成功。
INTERNAL_CLASSES = (
    "Winit Thread Event Target",
    "Winit Message Window",
    "Message",
    "OleMainThreadWndClass",
    "IME",
    "MSCTFIME UI",
    "Default IME",
)
MIN_W, MIN_H = 200, 150


def is_main_window(w):
    _hwnd, vis, w_, h_, cls, _ttl = w
    if not vis or w_ < MIN_W or h_ < MIN_H:
        return False
    return not any(k.lower() in cls.lower() for k in INTERNAL_CLASSES)


def windows_of(pid):
    out = []

    def cb(hwnd, _):
        p = wt.DWORD()
        u32.GetWindowThreadProcessId(hwnd, ctypes.byref(p))
        if p.value == pid:
            vis = bool(u32.IsWindowVisible(hwnd))
            r = wt.RECT()
            u32.GetWindowRect(hwnd, ctypes.byref(r))
            w, h = r.right - r.left, r.bottom - r.top
            cls = ctypes.create_unicode_buffer(256)
            u32.GetClassNameW(hwnd, cls, 256)
            ttl = ctypes.create_unicode_buffer(512)
            u32.GetWindowTextW(hwnd, ttl, 512)
            out.append((hwnd, vis, w, h, cls.value, ttl.value))
        return True

    u32.EnumWindows(WNDENUMPROC(cb), 0)
    return out


def main():
    exe = sys.argv[1]
    secs = float(sys.argv[2]) if len(sys.argv) > 2 else 40.0
    envspec = sys.argv[3] if len(sys.argv) > 3 else ""

    env = dict(os.environ)
    for kv in envspec.split(";"):
        if "=" in kv:
            k, v = kv.split("=", 1)
            env[k.strip()] = v

    print(f"被测：{exe}\n观察 {secs:.0f}s（每 2s 探一次）\n")
    p = subprocess.Popen([exe], env=env)
    print(f"pid = {p.pid}")

    t0 = time.time()
    found = None
    while time.time() - t0 < secs:
        if p.poll() is not None:
            print(f"\n★ 进程在建窗前就退出了：退出码 {p.returncode} "
                  f"(0x{p.returncode & 0xFFFFFFFF:08X})  耗时 {time.time()-t0:.1f}s")
            return 4
        wins = windows_of(p.pid)
        dlgs = [w for w in wins if w[1] and "#32770" in w[4]]
        if dlgs:
            print(f"\n★ {time.time()-t0:.1f}s 弹出了**模态对话框**（程序自己在报错）：")
            for hwnd, v, w, h, cls, ttl in dlgs:
                print(f"    hwnd=0x{hwnd:X}  {w}x{h}  class={cls!r}  title={ttl!r}")
            print("  → 这就是「打不开」的现场：窗口没建出来，程序弹框告知。")
            p.terminate()
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()
            print("已结束进程（对话框随之关闭）")
            return 5
        vis = [w for w in wins if is_main_window(w)]
        el = time.time() - t0
        if vis:
            print(f"\n✔ {el:.1f}s 出现**主窗口**（≥{MIN_W}x{MIN_H} 且非内部类）：")
            for hwnd, v, w, h, cls, ttl in vis:
                print(f"    hwnd=0x{hwnd:X}  {w}x{h}  class={cls!r}  title={ttl!r}")
            found = vis
            break
        print(f"  [{el:4.1f}s] 该进程顶层窗口 {len(wins)} 个，主窗口 0 个"
              + ("   全部（class, title, 尺寸）: "
                 + str([(c, t, f"{a}x{b}") for _, vv, a, b, c, t in wins][:6])
                 if wins else "（一个都没有）"))
        time.sleep(2)

    if found:
        # 再让它活一会儿，看窗口稳不稳
        time.sleep(3)
        if p.poll() is not None:
            print(f"★ 窗口出现后进程又退出了：退出码 {p.returncode}")
            return 3
        print("窗口保持稳定 ✔（再等 3s 仍在）")
        rc = 0
    else:
        print(f"\n★ {secs:.0f}s 内始终没有可见窗口")
        rc = 3

    p.terminate()
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        p.kill()
    print("已结束进程")
    return rc


if __name__ == "__main__":
    sys.exit(main())
