#!/usr/bin/env python3
"""T18 语料生成器：240 张分层契约卡（8 类×30），TOON 编码，确定性种子。

卡格式（TOON 文档，每卡一文件）：
  class: <类名>
  card_id: <类>-<序号>
  schema: <逗号分隔键>
  input: <抽取源文本>
  expect_<key>: <oracle 值>   （每 schema 键一条）

判定：closed parse + schema 键集闭合 + 值语义相等（规格化比较）。
"""
import random
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "t18_corpus"

# ── 8 类定义：schema + 生成器 ─────────────────────────────────────────
SUBJECTS = ["雨夜追踪", "实验室对峙", "屋顶告别", "地铁逃亡", "旧宅搜证", "码头交接",
            "天台谈判", "病房告白", "车库伏击", "档案室谜云", "剧院暗杀", "雪地追凶"]
ROLES = ["林澈", "苏晚", "陈默", "周野", "白汐", "顾岚", "赵一鸣", "程霜", "方淮", "沈砚"]
MOODS = ["紧张", "悲伤", "愤怒", "释然", "惊恐", "决绝", "温柔", "讥讽"]
PLACES = ["废弃工厂", "中央车站", "江边码头", "医院天台", "老城区巷子", "公司顶层", "地下车库", "美术馆"]

def gen_script(i: int, rng: random.Random) -> tuple[str, list[str], list[str], str]:
    subj, role, mood = rng.choice(SUBJECTS), rng.choice(ROLES), rng.choice(MOODS)
    place = rng.choice(PLACES)
    num = rng.randint(3, 28)
    inp = (f"场次记录：第{num}场「{subj}」。{role}在{place}与对手周旋，"
           f"整体情绪基调{mood}。导演要求本场以{role}的独白收尾。")
    keys = ["scene_no", "scene_title", "lead_role", "mood"]
    vals = [str(num), subj, role, mood]
    return inp, keys, vals, "剧本场记卡"

def gen_storyboard(i: int, rng: random.Random) -> tuple[str, list[str], list[str], str]:
    role, place = rng.choice(ROLES), rng.choice(PLACES)
    shot = rng.choice(["特写", "中景", "全景", "过肩", "俯拍", "手持跟拍"])
    secs = rng.choice([3, 5, 8, 12, 15])
    inp = (f"镜头指示：景别={shot}，主体={role}，场景={place}，时长={secs}秒，"
           f"镜头随主体移动缓慢推进，无切分。")
    keys = ["shot_type", "subject", "duration_sec", "location"]
    vals = [shot, role, str(secs), place]
    return inp, keys, vals, "分镜指令卡"

def gen_ledger(i: int, rng: random.Random) -> tuple[str, list[str], list[str], str]:
    binding = f"b{rng.randint(1, 40):03d}"
    op = rng.choice(["CreateProposal", "Grant", "Decision", "ActivateBegin", "Verify"])
    rev = rng.randint(0, 6)
    seq = rng.randint(1, 999)
    inp = (f"账本事件：seq={seq} 的记录对绑定 {binding} 执行 {op}，"
           f"修订号 rev={rev}，由 cli 发起，结果 OK。")
    keys = ["seq", "binding_id", "op", "revision"]
    vals = [str(seq), binding, op, str(rev)]
    return inp, keys, vals, "账本事件卡"

def gen_audit(i: int, rng: random.Random) -> tuple[str, list[str], list[str], str]:
    shot = f"shot_{rng.randint(100, 999)}"
    verdict = rng.choice(["PASS", "FAIL"])
    reason = rng.choice(["角色一致", "场景锚匹配", "无场景入侵", "帧率达标", "无闪烁"])
    score = rng.randint(60, 99)
    inp = f"质检结果：镜头 {shot} 判定 {verdict}，得分 {score}，原因：{reason}。"
    keys = ["shot_id", "verdict", "score", "reason"]
    vals = [shot, verdict, str(score), reason]
    return inp, keys, vals, "质检结果卡"

def gen_subtitle(i: int, rng: random.Random) -> tuple[str, list[str], list[str], str]:
    role = rng.choice(ROLES)
    start = rng.randint(0, 1800)
    dur = rng.choice([2, 3, 4, 5])
    line = rng.choice(["来不及了，走", "你从一开始就知道，对吧", "这不是你的战场",
                       "把东西交出来", "……原来是这样", "别回头"])
    inp = f"字幕条：{start} 秒起，时长 {dur} 秒，{role}说：{line}"
    keys = ["start_sec", "duration_sec", "speaker", "line"]
    vals = [str(start), str(dur), role, line]
    return inp, keys, vals, "字幕条目卡"

def gen_task(i: int, rng: random.Random) -> tuple[str, list[str], list[str], str]:
    kind = rng.choice(["场景图", "角色资产", "配音合成", "剪辑合成", "质检", "发布"])
    pri = rng.choice(["P0", "P1", "P2"])
    owner = rng.choice(ROLES)
    est = rng.randint(1, 48)
    inp = f"任务登记：任务类型={kind}，优先级 {pri}，负责人 {owner}，预计 {est} 小时。"
    keys = ["task_kind", "priority", "owner", "est_hours"]
    vals = [kind, pri, owner, str(est)]
    return inp, keys, vals, "任务登记卡"

def gen_review(i: int, rng: random.Random) -> tuple[str, list[str], list[str], str]:
    keep = rng.choice(["前 3 秒钩子", "雨夜转场", "主角特写", "结尾留白"])
    drop = rng.choice(["冗长铺垫", "重复闪回", "第二段独白", "过场空镜"])
    tone = rng.choice(["偏冷", "偏暖", "高对比", "低饱和"])
    inp = f"审校意见：保留项={keep}；删除项={drop}；色调={tone}。"
    keys = ["keep", "drop", "tone"]
    vals = [keep, drop, tone]
    return inp, keys, vals, "审校意见卡"

def gen_release(i: int, rng: random.Random) -> tuple[str, list[str], list[str], str]:
    title = rng.choice(["雨夜追踪", "第七层协议", "回声", "零点行动", "归档者"])
    plat = rng.choice(["dy", "xhs", "b站", "ks"])
    ep = rng.randint(1, 24)
    hh = rng.randint(9, 22)
    inp = f"发布单：短剧《{title}》第 {ep} 集发布至 {plat}，定档 {hh} 点整。"
    keys = ["drama_title", "episode", "platform", "hour"]
    vals = [title, str(ep), plat, str(hh)]
    return inp, keys, vals, "发布清单卡"

CLASSES = [
    ("script", gen_script), ("storyboard", gen_storyboard), ("ledger", gen_ledger),
    ("audit", gen_audit), ("subtitle", gen_subtitle), ("task", gen_task),
    ("review", gen_review), ("release", gen_release),
]


def toon_str(v: str) -> str:
    # TOON 标量：需要引号的形式（空/首尾空格/含定界符/数字形/bool 形）加引号
    need = (not v) or v != v.strip() or any(c in v for c in ",:[]{}#\"'\\ \n\t") \
        or v in ("true", "false", "null") \
        or v.replace("-", "", 1).replace(".", "", 1).isdigit()
    if not need:
        return v
    esc = v.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\t", "\\t")
    return f'"{esc}"'


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    n = 0
    for cls, gen in CLASSES:
        for i in range(30):
            rng = random.Random(f"t18-{cls}-{i}")  # 确定性：类+序号=种子
            inp, keys, vals, _desc = gen(i, rng)
            lines = [
                f"class: {cls}",
                f"card_id: {cls}-{i + 1:02d}",
                f"schema: {toon_str(','.join(keys))}",
                f"input: {toon_str(inp)}",
            ]
            for k, v in zip(keys, vals):
                lines.append(f"expect_{k}: {toon_str(v)}")
            card = "\n".join(lines) + "\n"
            (OUT / f"{cls}-{i + 1:02d}.toon").write_text(card, encoding="utf-8")
            n += 1
    print(f"generated {n} cards -> {OUT}")


if __name__ == "__main__":
    main()
