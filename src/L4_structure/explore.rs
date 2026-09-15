//! Explore — v0.19 探索环驱动器（explore-then-freeze）。
//!
//! 规格：docs/v0.19_explore_loop_spec.md（用户定调 2026-09-15，RSIAgent 对照）。
//!
//! 三角色映射：curriculum（LLM 出题，[agents.curriculum]）→ actor（现有
//! executor 跑探针管线）→ verifier（确定性判据优先）。两阶段：BRS 并行波 +
//! DRS 串行递进。循环在引擎不在模型：预算、收敛判定、失败分类全在本模块，
//! 可单测、可进回归门禁。零默认面：不调 explore 现有管线零变化。
//!
//! 本模块 P0 范围 = 契约解析 + 循环骨架（纯函数，无 IO）。
//! 探针执行接线（executor 沙箱调用）在 P1。

use std::collections::BTreeMap;

/// 一道探针题（curriculum 产出）。
#[derive(Debug, Clone, PartialEq)]
pub struct ExploreTask {
    pub id: String,
    pub goal: String,
    /// broad（BRS 波内并行）| deep（DRS 串行递进）
    pub kind: TaskKind,
    /// DSL 片段或自然语言步骤（P0 不执行，P1 喂 executor）
    pub plan: String,
    /// 确定性判据描述（grep 什么 / exit code 期望 / parse 输出含什么）
    pub judge: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Broad,
    Deep,
}

impl TaskKind {
    pub fn parse(s: &str) -> Option<TaskKind> {
        match s.trim() {
            "broad" => Some(TaskKind::Broad),
            "deep" => Some(TaskKind::Deep),
            _ => None,
        }
    }
}

/// curriculum 一轮输出契约（§2）。
/// 硬错误（fail-closed）：tasks 空 + stop=false = 要不出题又不说停。
#[derive(Debug, Clone, PartialEq)]
pub struct CurriculumOutput {
    pub tasks: Vec<ExploreTask>,
    /// 本轮探针针对的记忆缺口
    pub gap: String,
    pub stop: bool,
}

/// 解析 curriculum LLM 输出（手写 JSON，仓库不引 serde——与 negotiate 同款）。
/// 契约：{"tasks":[{"id":..,"goal":..,"kind":"broad|deep","plan":..,"judge":..}],
///        "gap":.., "stop":bool}
/// 解析失败 / 契约违约 → Err（fail-closed，不静默降级）。
pub fn parse_curriculum_json(text: &str) -> Result<CurriculumOutput, String> {
    // 容错定位 JSON 对象体（模型可能带 markdown 围栏/前后缀文本）
    let start = text.find('{').ok_or("curriculum: 输出无 JSON 对象")?;
    let end = match text.rfind('}') {
        Some(e) if e > start => e,
        _ => return Err("curriculum: JSON 对象未闭合".into()),
    };
    let body = &text[start..=end];

    let stop = extract_json_bool(body, "stop").unwrap_or(false);
    let gap = extract_json_str_field(body, "gap").unwrap_or_default();

    // tasks 数组：找 "tasks": [ ... ] 顶层区间（深度感知）
    let arr_start = body
        .find("\"tasks\"")
        .and_then(|p| body[p..].find('[').map(|q| p + q))
        .ok_or("curriculum: 契约缺 tasks 字段")?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut arr_end = None;
    for (i, c) in body[arr_start..].char_indices() {
        match c {
            '"' => in_str = !in_str,
            '[' if !in_str => depth += 1,
            ']' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    arr_end = Some(arr_start + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let arr_end = arr_end.ok_or("curriculum: tasks 数组未闭合")?;
    let arr = &body[arr_start..=arr_end];

    // 逐对象切分（深度感知，字符串内不计——negotiate 花括号教训同款纪律）
    let mut tasks = Vec::new();
    let mut rest = arr;
    while let Some(obj_start) = rest.find('{') {
        let mut odepth = 0i32;
        let mut ostr = false;
        let mut obj_end = None;
        for (i, c) in rest[obj_start..].char_indices() {
            match c {
                '"' => ostr = !ostr,
                '{' if !ostr => odepth += 1,
                '}' if !ostr => {
                    odepth -= 1;
                    if odepth == 0 {
                        obj_end = Some(obj_start + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let obj_end = match obj_end {
            Some(e) => e,
            None => break,
        };
        let obj = &rest[obj_start..=obj_end];
        let id = extract_json_str_field(obj, "id").ok_or("curriculum: task 缺 id")?;
        let goal = extract_json_str_field(obj, "goal").ok_or("curriculum: task 缺 goal")?;
        let kind_s = extract_json_str_field(obj, "kind").ok_or("curriculum: task 缺 kind")?;
        let kind = TaskKind::parse(&kind_s)
            .ok_or_else(|| format!("curriculum: task {} kind 非法 {:?}", id, kind_s))?;
        let plan = extract_json_str_field(obj, "plan").ok_or("curriculum: task 缺 plan")?;
        let judge = extract_json_str_field(obj, "judge").ok_or("curriculum: task 缺 judge")?;
        if judge.trim().is_empty() {
            return Err(format!(
                "curriculum: task {} judge 为空（判据必须确定性可判）",
                id
            ));
        }
        tasks.push(ExploreTask {
            id,
            goal,
            kind,
            plan,
            judge,
        });
        rest = &rest[obj_end + 1..];
    }

    if tasks.is_empty() && !stop {
        return Err("curriculum: 契约违约——tasks 空且 stop=false（要不出题又不说停）".into());
    }
    Ok(CurriculumOutput { tasks, gap, stop })
}

/// JSON 字符串字段提取（首个匹配，含 \" 转义感知）。失败 → None。
fn extract_json_str_field(body: &str, field: &str) -> Option<String> {
    let key = format!("\"{}\"", field);
    let kpos = body.find(&key)?;
    let colon = body[kpos..].find(':')? + kpos;
    let q1 = body[colon..].find('"')? + colon;
    let mut out = String::new();
    let mut chars = body[q1 + 1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(esc) = chars.next() {
                    out.push(match esc {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                }
            }
            '"' => return Some(out),
            other => out.push(other),
        }
    }
    None
}

/// JSON 布尔字段提取。失败 → None。
fn extract_json_bool(body: &str, field: &str) -> Option<bool> {
    let key = format!("\"{}\"", field);
    let kpos = body.find(&key)?;
    let colon = body[kpos..].find(':')? + kpos;
    let rest = body[colon + 1..].trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// 探索预算（§3）。
#[derive(Debug, Clone, Copy)]
pub struct ExploreBudget {
    /// BRS 波数上限（波间检查，不打断进行中的波）
    pub waves: usize,
    /// DRS 步数上限
    pub deep_steps: usize,
}

impl Default for ExploreBudget {
    fn default() -> Self {
        ExploreBudget {
            waves: 8,
            deep_steps: 12,
        }
    }
}

/// 探索终止原因。
#[derive(Debug, Clone, PartialEq)]
pub enum ExploreStop {
    Budget,
    CurriculumStop,
    /// fail-closed：curriculum 契约违约 / 判据不可判 / 探针执行非法
    FailClosed(String),
}

/// 一道题的执行结果（verifier 判定）。
#[derive(Debug, Clone, PartialEq)]
pub enum TaskOutcome {
    /// 确定性判据通过（按 judge 描述的 exit code / 文件 / AST 检查）
    Pass { evidence: String },
    /// 确定性判据失败——暴露缺陷/隐藏约束（探索的正产出）
    Fail { evidence: String },
    /// 判据不可判（如 judge 描述的文件不存在）→ fail-closed 通道
    Undecidable { reason: String },
}

/// 探索报告（落 runs 表，管线名 explore:<topic>，§4）。
#[derive(Debug, Clone, Default)]
pub struct ExploreReport {
    pub topic: String,
    pub waves_run: usize,
    pub deep_steps_run: usize,
    /// 每题：id → (kind, outcome)
    pub results: BTreeMap<String, (TaskKind, TaskOutcome)>,
    /// 固化清单（incident/patch/composition 指针，P1 接线）
    pub consolidated: Vec<String>,
    pub stop_reason: Option<ExploreStop>,
    pub frozen: bool,
}

impl ExploreReport {
    /// 门禁 4：预算耗尽必标 Budget，不静默成功。
    pub fn budget_exhausted(&self) -> bool {
        matches!(self.stop_reason, Some(ExploreStop::Budget))
    }
    /// 门禁 2 验收辅助：探索有正产出（发现=Fail 或固化非空）。
    pub fn has_findings(&self) -> bool {
        !self.consolidated.is_empty()
            || self
                .results
                .values()
                .any(|(_, o)| matches!(o, TaskOutcome::Fail { .. }))
    }
}

/// 阶段开关（§3：stages="brs" / "drs" / "brs,drs"，默认全开）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stages {
    pub brs: bool,
    pub drs: bool,
}

impl Default for Stages {
    fn default() -> Self {
        Stages {
            brs: true,
            drs: true,
        }
    }
}

impl Stages {
    pub fn parse(s: &str) -> Result<Stages, String> {
        let mut st = Stages {
            brs: false,
            drs: false,
        };
        if s.trim().is_empty() {
            return Ok(Stages::default());
        }
        for part in s.split(',') {
            match part.trim() {
                "brs" => st.brs = true,
                "drs" => st.drs = true,
                other => return Err(format!("explore: stages 非法 {:?}", other)),
            }
        }
        Ok(st)
    }
}

// ── 驱动循环（P0 核心：编排逻辑，依赖经闭包注入——LLM/executor 在 CLI 层接线，
// 测试注入 mock。循环在引擎不在模型：预算/终止/违约判定全在此，可单测）──

/// curriculum 回调：读 memory 概况 + 上轮结果 → 本轮题目（或 stop）。
pub type CurriculumFn<'a> = dyn FnMut(&ExploreReport) -> Result<CurriculumOutput, String> + 'a;
/// 探针执行回调：跑一道题，回原始产物（stdout/exit code/parse 输出）。
pub type ExecuteFn<'a> = dyn FnMut(&ExploreTask) -> Result<String, String> + 'a;
/// 确定性裁判回调：judge 描述 + 产物 → 判定。
pub type JudgeFn<'a> = dyn FnMut(&ExploreTask, &str) -> TaskOutcome + 'a;

/// 完整探索环（§3 两阶段）。
/// - BRS：curriculum 出题（kind=broad 过滤），逐波执行+判定，波间预算检查
/// - DRS：串行递进（kind=deep），每步结果回灌 curriculum
/// - 终止：curriculum stop / 预算尽 / fail-closed（违约即返，report 记因）
pub fn run_explore_loop(
    topic: &str,
    budget: &ExploreBudget,
    stages: &Stages,
    curriculum: &mut CurriculumFn,
    execute: &mut ExecuteFn,
    judge: &mut JudgeFn,
) -> ExploreReport {
    let mut report = ExploreReport {
        topic: topic.to_string(),
        ..Default::default()
    };

    // ── BRS 宽探索 ──
    if stages.brs {
        while report.waves_run < budget.waves {
            let round = match curriculum(&report) {
                Ok(r) => r,
                Err(e) => {
                    report.stop_reason = Some(ExploreStop::FailClosed(e));
                    return report;
                }
            };
            if round.stop {
                report.stop_reason = Some(ExploreStop::CurriculumStop);
                break;
            }
            let broad: Vec<ExploreTask> = round
                .tasks
                .iter()
                .filter(|t| t.kind == TaskKind::Broad)
                .cloned()
                .collect();
            if broad.is_empty() {
                // 本轮只出了 deep 题：BRS 无事可做，交给 DRS（避免空波烧预算）
                break;
            }
            for t in broad {
                let outcome = run_one(t.clone(), execute, judge);
                report.results.insert(t.id.clone(), (t.kind, outcome));
            }
            report.waves_run += 1;
            if report.waves_run >= budget.waves {
                report.stop_reason = Some(ExploreStop::Budget);
            }
        }
    }

    // ── DRS 深探索 ──
    if stages.drs {
        while report.deep_steps_run < budget.deep_steps {
            let round = match curriculum(&report) {
                Ok(r) => r,
                Err(e) => {
                    report.stop_reason = Some(ExploreStop::FailClosed(e));
                    return report;
                }
            };
            if round.stop {
                report.stop_reason = Some(ExploreStop::CurriculumStop);
                break;
            }
            let deep = match round.tasks.iter().find(|t| t.kind == TaskKind::Deep) {
                Some(t) => t.clone(),
                None => break, // 无 deep 题 = curriculum 隐式收敛
            };
            let outcome = run_one(deep.clone(), execute, judge);
            report.results.insert(deep.id.clone(), (deep.kind, outcome));
            report.deep_steps_run += 1;
            if report.deep_steps_run >= budget.deep_steps {
                report.stop_reason = Some(ExploreStop::Budget);
            }
        }
    }

    report
}

fn run_one(task: ExploreTask, execute: &mut ExecuteFn, judge: &mut JudgeFn) -> TaskOutcome {
    match execute(&task) {
        Ok(artifact) => judge(&task, &artifact),
        Err(e) => TaskOutcome::Undecidable { reason: e },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curriculum_json(tasks: &str, gap: &str, stop: bool) -> String {
        format!(
            "{{\"tasks\": [{}], \"gap\": \"{}\", \"stop\": {}}}",
            tasks, gap, stop
        )
    }

    #[test]
    fn parse_basic_two_tasks() {
        let raw = curriculum_json(
            "{\"id\":\"probe-a\",\"goal\":\"验证A\",\"kind\":\"broad\",\"plan\":\"run(echo A)\",\"judge\":\"stdout 含 A\"},\
             {\"id\":\"probe-b\",\"goal\":\"验证B\",\"kind\":\"deep\",\"plan\":\"parse(x.pipeline)\",\"judge\":\"exit 0\"}",
            "foreach 依赖边覆盖",
            false,
        );
        let out = parse_curriculum_json(&raw).unwrap();
        assert_eq!(out.tasks.len(), 2);
        assert_eq!(out.tasks[0].kind, TaskKind::Broad);
        assert_eq!(out.tasks[1].kind, TaskKind::Deep);
        assert!(!out.stop);
        assert_eq!(out.gap, "foreach 依赖边覆盖");
    }

    #[test]
    fn parse_with_markdown_fence_and_prose() {
        // 模型带围栏+前后缀文本的容错
        let raw = "好的，以下是本轮探针：\n```json\n{\"tasks\": [{\"id\":\"p1\",\"goal\":\"g\",\"kind\":\"broad\",\"plan\":\"x\",\"judge\":\"y\"}], \"gap\":\"\", \"stop\": false}\n```\n以上。";
        let out = parse_curriculum_json(raw).unwrap();
        assert_eq!(out.tasks.len(), 1);
        assert_eq!(out.tasks[0].id, "p1");
    }

    #[test]
    fn contract_violation_empty_tasks_no_stop() {
        // 门禁 3：tasks 空 + stop=false = 硬错误
        let raw = curriculum_json("", "没缺口了", false);
        let err = parse_curriculum_json(&raw).unwrap_err();
        assert!(err.contains("违约"), "{}", err);
    }

    #[test]
    fn stop_true_with_empty_tasks_ok() {
        let raw = curriculum_json("", "收敛", true);
        let out = parse_curriculum_json(&raw).unwrap();
        assert!(out.stop);
        assert!(out.tasks.is_empty());
    }

    #[test]
    fn missing_judge_is_hard_error() {
        // 判据必须非空（§2：judge 必须确定性可判）
        let raw = curriculum_json(
            "{\"id\":\"p1\",\"goal\":\"g\",\"kind\":\"broad\",\"plan\":\"x\",\"judge\":\"\"}",
            "",
            false,
        );
        assert!(parse_curriculum_json(&raw).is_err());
    }

    #[test]
    fn bad_kind_rejected() {
        let raw = curriculum_json(
            "{\"id\":\"p1\",\"goal\":\"g\",\"kind\":\"wide\",\"plan\":\"x\",\"judge\":\"y\"}",
            "",
            false,
        );
        assert!(parse_curriculum_json(&raw).is_err());
    }

    #[test]
    fn braces_inside_plan_do_not_truncate() {
        // negotiate 花括号截断教训：plan 含 {..} 嵌套
        let raw = curriculum_json(
            "{\"id\":\"p1\",\"goal\":\"验证 foreach\",\"kind\":\"broad\",\"plan\":\"foreach(source=@x, var=i){body}\",\"judge\":\"exit 0\"},\
             {\"id\":\"p2\",\"goal\":\"验证 .when\",\"kind\":\"deep\",\"plan\":\"when(a=={b})\",\"judge\":\"parse ok\"}",
            "",
            false,
        );
        let out = parse_curriculum_json(&raw).unwrap();
        assert_eq!(out.tasks.len(), 2, "花括号不应截断对象: {:?}", out.tasks);
        assert_eq!(out.tasks[1].id, "p2");
    }

    #[test]
    fn escaped_quotes_in_goal() {
        let raw = curriculum_json(
            "{\"id\":\"p1\",\"goal\":\"验证 \\\"引号\\\" 场景\",\"kind\":\"broad\",\"plan\":\"x\",\"judge\":\"y\"}",
            "",
            false,
        );
        let out = parse_curriculum_json(&raw).unwrap();
        assert!(
            out.tasks[0].goal.contains("引号"),
            "{:?}",
            out.tasks[0].goal
        );
    }

    #[test]
    fn stages_parse() {
        assert!(Stages::parse("").unwrap().brs && Stages::parse("").unwrap().drs);
        let s = Stages::parse("brs").unwrap();
        assert!(s.brs && !s.drs);
        assert!(Stages::parse("brs,drs").unwrap().drs);
        assert!(Stages::parse("wide").is_err());
    }

    // ── 驱动循环（mock 注入，门禁 3/4 验收路径）──

    fn task(id: &str, kind: TaskKind) -> ExploreTask {
        ExploreTask {
            id: id.into(),
            goal: format!("goal-{}", id),
            kind,
            plan: format!("plan-{}", id),
            judge: "exit 0".into(),
        }
    }

    #[test]
    fn loop_brs_then_drs_full_cycle() {
        // 门禁 2 骨架：BRS>=2 波 + DRS>=2 步，报告完整
        let mut wave = 0;
        let mut curriculum_calls = 0;
        let mut curriculum = |_r: &ExploreReport| {
            curriculum_calls += 1;
            wave += 1;
            if wave <= 2 {
                Ok(CurriculumOutput {
                    tasks: vec![task(&format!("b{}", wave), TaskKind::Broad)],
                    gap: "g".into(),
                    stop: false,
                })
            } else if wave <= 5 {
                // BRS 完（无 broad 题），DRS 出 2 步
                Ok(CurriculumOutput {
                    tasks: vec![task(&format!("d{}", wave), TaskKind::Deep)],
                    gap: "g".into(),
                    stop: false,
                })
            } else {
                Ok(CurriculumOutput {
                    tasks: vec![],
                    gap: "".into(),
                    stop: true,
                })
            }
        };
        let mut execute = |t: &ExploreTask| Ok(format!("artifact-{}", t.id));
        let mut judge = |t: &ExploreTask, a: &str| {
            if t.id.contains("d") {
                TaskOutcome::Fail { evidence: a.into() } // 深探发现缺陷
            } else {
                TaskOutcome::Pass { evidence: a.into() }
            }
        };
        let r = run_explore_loop(
            "selfaudit",
            &ExploreBudget {
                waves: 8,
                deep_steps: 12,
            },
            &Stages::default(),
            &mut curriculum,
            &mut execute,
            &mut judge,
        );
        assert_eq!(r.waves_run, 2, "两波 BRS");
        assert_eq!(r.deep_steps_run, 2, "两步 DRS");
        assert!(matches!(r.stop_reason, Some(ExploreStop::CurriculumStop)));
        assert!(r.has_findings(), "DRS Fail 是正产出");
        assert!(curriculum_calls >= 5);
    }

    #[test]
    fn loop_budget_gate_marks_budget() {
        // 门禁 4：预算耗尽必标 Budget 不静默
        let mut n = 0;
        let mut curriculum = move |_r: &ExploreReport| {
            n += 1;
            Ok(CurriculumOutput {
                tasks: vec![task(&format!("b{}", n), TaskKind::Broad)],
                gap: "".into(),
                stop: false,
            })
        };
        let mut execute = |_t: &ExploreTask| Ok("a".into());
        let mut judge = |_t: &ExploreTask, _a: &str| TaskOutcome::Pass {
            evidence: "e".into(),
        };
        let r = run_explore_loop(
            "t",
            &ExploreBudget {
                waves: 3,
                deep_steps: 0,
            },
            &Stages {
                brs: true,
                drs: false,
            },
            &mut curriculum,
            &mut execute,
            &mut judge,
        );
        assert_eq!(r.waves_run, 3);
        assert!(r.budget_exhausted(), "必须标 Budget");
        assert!(!r.has_findings());
    }

    #[test]
    fn loop_contract_violation_fails_closed() {
        // 门禁 3：curriculum 违约（解析层已拦）与运行层 Err 都 fail-closed
        let mut curriculum = |_r: &ExploreReport| Err("llm 桥挂了".to_string());
        let mut execute = |_t: &ExploreTask| Ok("a".into());
        let mut judge = |_t: &ExploreTask, _a: &str| TaskOutcome::Pass {
            evidence: "e".into(),
        };
        let r = run_explore_loop(
            "t",
            &ExploreBudget::default(),
            &Stages::default(),
            &mut curriculum,
            &mut execute,
            &mut judge,
        );
        assert!(matches!(r.stop_reason, Some(ExploreStop::FailClosed(_))));
        assert_eq!(r.waves_run, 0, "违约即停，不烧预算");
    }

    #[test]
    fn loop_execute_error_is_undecidable() {
        let mut wave = 0;
        let mut curriculum = move |_r: &ExploreReport| {
            wave += 1;
            if wave == 1 {
                Ok(CurriculumOutput {
                    tasks: vec![task("b1", TaskKind::Broad)],
                    gap: "".into(),
                    stop: false,
                })
            } else {
                Ok(CurriculumOutput {
                    tasks: vec![],
                    gap: "".into(),
                    stop: true,
                })
            }
        };
        let mut execute = |_t: &ExploreTask| Err("探针管线不存在".to_string());
        let mut judge = |_t: &ExploreTask, _a: &str| TaskOutcome::Pass {
            evidence: "e".into(),
        };
        let r = run_explore_loop(
            "t",
            &ExploreBudget::default(),
            &Stages::default(),
            &mut curriculum,
            &mut execute,
            &mut judge,
        );
        assert!(matches!(
            r.results.get("b1"),
            Some((_, TaskOutcome::Undecidable { .. }))
        ));
    }

    #[test]
    fn report_gates() {
        let mut r = ExploreReport::default();
        assert!(!r.has_findings());
        r.results.insert(
            "p1".into(),
            (
                TaskKind::Broad,
                TaskOutcome::Fail {
                    evidence: "e".into(),
                },
            ),
        );
        assert!(r.has_findings());
        r.stop_reason = Some(ExploreStop::Budget);
        assert!(r.budget_exhausted());
        // Pass-only 且无固化 = 无发现（探索空转，P3 自举判据的输入）
        let mut r2 = ExploreReport::default();
        r2.results.insert(
            "p2".into(),
            (
                TaskKind::Deep,
                TaskOutcome::Pass {
                    evidence: "e".into(),
                },
            ),
        );
        assert!(!r2.has_findings());
    }
}
