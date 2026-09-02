#!/usr/bin/env python3
"""从 clipknow.db 造一个精简的 eval 基准库。

为什么要单独造一份，而不是直接拷 clipknow.db：

**① 每次运行都要重置，所以必须小。** clipknow.db 是 66MB，其中 63.4MB 是
items 表（会话历史里的材料原文），占 96%。删掉会话之后只剩约 3MB，
每次运行前拷一份基本免费。

**② eval 的会话不该混进你的历史。** 基准库不带任何 sessions/turns/items，
每次运行都是一个干净的会话空间。

**③ 视频资料必须留着。** 缓存命中的题（A1/A2/C1/F1/F2…）靠的就是库里
已经有那 27 条视频的文字稿、评论、视觉档案。
"""
import pathlib
import shutil
import sqlite3
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SRC = ROOT / "clipknow.db"
OUT = ROOT / "evals" / "baseline.db"

# 一段超过 40000 字符的文字稿，用来测截断。
#
# 库里最长的文字稿只有 7889 字符（40 分钟那条也才 7849——SC 的文字稿是语音
# 转录，不是逐帧），离 LOOP_TRANSCRIPT_LIMIT_CHARS = 40000 差五倍。也就是说
# **截断那条代码路径从来没被测过，靠现有数据也不可能测到**。只能造。
LONG_TRANSCRIPT_NATIVE_ID = "7659357354554641695"  # @lcsign_lightbox
#
# ★ 挑这一条是因为它有**通用档案**（question IS NULL），不是随便有档案就行。
#
#   踩过两次：
#   ① 先放在 @espn 上——那条一份档案都没有，fetch_video 顺带做了一次通用视觉
#      分析，要拿 CDN 直链，直链过期又打了 1 次详情端点。
#   ② 换到 @houseofhighlights——它有 1 份档案，但那是**带问题的**档案。
#      latest_general_dossier 的条件是 `question IS NULL`，带问题的档案接不住
#      「这条视频讲什么」这种通用请求，于是照样走 ④ 全量、照样 +1 次调用。
#
#   库里 27 条视频只有 6 条有通用档案。这条（24 秒）是其中没被别的题用到的。
#
# 内容做成**有变化的**，不是同一句重复。第一版是一句话重复 500 遍，模型一眼
#   识破「这是垃圾数据，和视频内容对不上」并花了半篇答案讲这件事——那让这道题
#   同时在测两件事，意图就浑了。
_TOPICS = [
    "外线的挡拆处理", "禁区的护框选择", "转换进攻的推进速度", "弱侧的补防轮转",
    "低位单打的脚步", "三分线外的空间创造", "篮板保护的站位", "全场紧逼的换防",
    "快攻中的传球时机", "关键球的战术执行",
]
_DETAIL = [
    "这一次的处理明显比上半场干净，球出手前多了一次观察。",
    "防守端的沟通还是慢半拍，弧顶停球的时间偏长。",
    "教练在暂停里专门讲了这个位置的选择，回来之后调整到位了。",
    "从慢放看，第一步的方向就已经决定了后面的结果。",
    "这一段的失误更多来自节奏，而不是个人能力。",
]


def long_transcript(min_chars=45000):
    out, n, i = [], 0, 1
    while n < min_chars:
        piece = (f"第 {i} 节。这一段讲{_TOPICS[i % len(_TOPICS)]}。"
                 f"{_DETAIL[i % len(_DETAIL)]}"
                 f"比分来到 {60 + i % 40}-{58 + i % 37}，还剩 {12 - i % 12} 分钟。")
        out.append(piece)
        n += len(piece)
        i += 1
    return "".join(out)


def main():
    if not SRC.exists():
        sys.exit(f"找不到 {SRC}")
    for suffix in ("", "-wal", "-shm"):
        p = pathlib.Path(str(OUT) + suffix)
        if p.exists():
            p.unlink()
    shutil.copy(SRC, OUT)

    con = sqlite3.connect(OUT)
    before = OUT.stat().st_size

    # 删会话即可——turns 和 items 有 ON DELETE CASCADE
    con.execute("PRAGMA foreign_keys = ON")
    con.execute("DELETE FROM sessions")
    con.commit()

    # 造那段超长文字稿
    row = con.execute("SELECT id FROM videos WHERE native_id = ?",
                      (LONG_TRANSCRIPT_NATIVE_ID,)).fetchone()
    if row:
        txt = long_transcript()
        con.execute("DELETE FROM transcripts WHERE video_id = ?", (row[0],))
        con.execute(
            "INSERT INTO transcripts (video_id, text, source, lang, fetched_at)"
            " VALUES (?,?,?,?,strftime('%s','now'))",
            (row[0], txt, "eval-long", "zh"))
        con.commit()
        print(f"  已造一段 {len(txt)} 字符的文字稿（@lcsign_lightbox），用来测 40000 字符截断")
    else:
        print(f"  ⚠ 库里没有 native_id={LONG_TRANSCRIPT_NATIVE_ID}，跳过超长文字稿")

    con.execute("VACUUM")
    con.close()
    after = OUT.stat().st_size

    con = sqlite3.connect(OUT)
    counts = {t: con.execute(f"SELECT count(*) FROM {t}").fetchone()[0]
              for t in ("videos", "artifacts", "transcripts", "comments",
                        "video_dossiers", "sessions", "turns", "items")}
    con.close()
    print(f"  {before/1048576:.1f} MB → {after/1048576:.1f} MB")
    print("  " + " · ".join(f"{k} {v}" for k, v in counts.items()))
    print(f"\n基准库：{OUT}")


if __name__ == "__main__":
    main()
