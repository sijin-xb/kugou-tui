#!/usr/bin/env python3
"""端到端渲染验证：在真实 pty 里跑 kugou-tui，对原始输出字节做断言。

这个脚本存在的意义：TestBackend 上的单元测试验证不了「字节真的到了终端、
以正确的顺序、没有被缓冲吃掉」。2026-11-14 的同步渲染回归（在 draw 之后
误调 Terminal::flush，导致每帧渲染完被 diff 擦除、整屏常空）在所有单元
测试全绿的情况下漏掉了——只有真实 pty 字节能抓住它。

断言：
1. BEGIN/END (DEC 2026) 严格交替且数量配对；
2. 相邻两帧之间（END 到下一个 BEGIN 之间）没有任何字节——帧外不允许
   有输出（那次回归的擦屏字节就落在帧外）；
3. 输出里出现真实界面文本（面板标题），且首帧字节量足够大。

用法：
    python3 tools/e2e_render_pty.py [--bin PATH] [--rows N] [--cols N] [--seconds S]

退出码 0 = 全部断言通过；非 0 = 有断言失败（打印原因）。
"""

import argparse
import fcntl
import os
import pty
import re
import select
import signal
import struct
import sys
import termios
import time

BEGIN = b"\x1b[?2026h"
END = b"\x1b[?2026l"


def run(binary: str, rows: int, cols: int, seconds: float) -> bytes:
    master, slave = pty.openpty()
    winsize = struct.pack("HHHH", rows, cols, 0, 0)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, winsize)
    fcntl.ioctl(master, termios.TIOCSWINSZ, winsize)

    pid = os.fork()
    if pid == 0:  # 子进程：pty 作为标准流，exec 目标程序
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        os.dup2(slave, 0)
        os.dup2(slave, 1)
        os.dup2(slave, 2)
        if slave > 2:
            os.close(slave)
        os.close(master)
        os.execv(binary, [binary])
        os._exit(127)

    os.close(slave)
    chunks = []
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        ready, _, _ = select.select([master], [], [], 0.2)
        if ready:
            try:
                chunk = os.read(master, 65536)
            except OSError:
                break
            if not chunk:
                break
            chunks.append(chunk)
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
    os.close(master)
    return b"".join(chunks)


def check(data: bytes) -> list[str]:
    failures = []

    begins = [m.start() for m in re.finditer(re.escape(BEGIN), data)]
    ends = [m.start() for m in re.finditer(re.escape(END), data)]
    if len(begins) == 0:
        failures.append("没有任何 BEGIN (2026h) 序列——同步渲染没有生效")
        return failures

    # 1. 配对：第 n 个 BEGIN 之后必须有一个 END，且严格交替
    if len(ends) < len(begins) - 1:
        failures.append(
            f"BEGIN({len(begins)}) 与 END({len(ends)}) 数量不配对——存在未关闭的同步窗口，"
            "终端会挂起后续输出"
        )
    for i, begin_at in enumerate(begins):
        if i < len(ends) and ends[i] < begin_at:
            failures.append(f"第 {i} 帧的 END 出现在 BEGIN 之前（顺序错乱）")
            break

    # 2. 帧外无输出：每个 END 到下一个 BEGIN 之间必须没有字节
    for i, end_at in enumerate(ends):
        next_begin = next((b for b in begins if b > end_at), None)
        if next_begin is None:
            break
        gap = data[end_at + len(END) : next_begin]
        if gap:
            failures.append(
                f"第 {i} 帧与第 {i + 1} 帧之间有 {len(gap)} 字节输出"
                f"（帧外输出，往往是误用 Terminal::flush 造成的擦屏）：{gap[:60]!r}"
            )
            break

    # 3. 真实界面内容：先剥掉 ANSI 控制序列再找文本——渲染时每个格子的样式
    # 变化都会插入转义序列，直接在原始流里找子串必然失败。
    text = re.sub(
        rb"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07|\x1b[()][0-9A-B]",
        b"",
        data,
    ).decode("utf-8", errors="replace")
    for marker in ("播放", "首页"):
        if marker not in text:
            failures.append(f"输出里没有界面文本「{marker}」——界面没有被画出来")
    first_end = ends[0] if ends else len(data)
    first_frame = data[begins[0] : first_end]
    if len(first_frame) < 500:
        failures.append(
            f"首帧只有 {len(first_frame)} 字节——整屏内容没有被画出来"
            "（缓冲为空？尺寸探测失败？）"
        )

    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    parser.add_argument("--bin", default=os.path.join(repo, "target", "release", "kugou-tui"))
    parser.add_argument("--rows", type=int, default=36)
    parser.add_argument("--cols", type=int, default=120)
    parser.add_argument("--seconds", type=float, default=3.0)
    args = parser.parse_args()

    print(f"== e2e 渲染验证：{args.bin} @ {args.cols}x{args.rows}，{args.seconds}s ==")
    data = run(args.bin, args.rows, args.cols, args.seconds)
    print(f"捕获 {len(data)} 字节")

    failures = check(data)
    if failures:
        for failure in failures:
            print(f"✗ {failure}")
        return 1
    print("✓ BEGIN/END 严格配对交替")
    print("✓ 帧与帧之间无额外输出（没有擦屏字节）")
    print("✓ 界面内容真实落在输出里（播放/首页）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
