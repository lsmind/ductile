//! TUI 视图——数据装载 + ratatui 渲染。纯读侧。

use super::app::{App, Tab};
use super::theme::*;
use crate::L0_physical::db;
use crate::L1_feedback::{incident, l4};
use crate::L3_dsl::parser;
use crate::L4_structure::{harvest, hyper};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

// ── 数据装载 ──

pub struct StatusSnapshot {
    pub incidents_open: usize,
    pub incidents_total: usize,
    pub degraded: Vec<String>,
    pub l4_phase: String,
    pub l4_agreement: Option<f64>,
    pub canary_total: i64,
    pub canary_pass: i64,
}

pub fn load_status() -> StatusSnapshot {
    let conn = db::open();
    let incidents = incident::list_incidents_conn(&conn, None);
    let open = incidents.iter().filter(|i| i.status == "open").count();
    let phase = l4::phase_for_conn(&conn).as_str().to_string();
    let agreement = l4::agreement_rate(&conn);
    let (ct, cp) = canary_counts(&conn);
    StatusSnapshot {
        incidents_open: open,
        incidents_total: incidents.len(),
        degraded: harvest::list_degraded(),
        l4_phase: phase,
        l4_agreement: agreement,
        canary_total: ct,
        canary_pass: cp,
    }
}

fn canary_counts(conn: &rusqlite::Connection) -> (i64, i64) {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(pass),0) FROM canary_runs",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap_or((0, 0))
}

pub struct RunRowUi {
    pub proc: String,
    pub impl_: String,
    pub status: String,
    pub latency_ms: i64,
    pub at: String,
    pub tokens: i64,
}

pub fn load_runs(limit: usize) -> Vec<RunRowUi> {
    db::recent_runs_limit("", limit)
        .into_iter()
        .map(|r| RunRowUi {
            proc: r.proc_name,
            impl_: r.impl_name,
            status: r.status,
            latency_ms: r.latency_ms,
            at: r.recorded_at,
            tokens: r.rate_tokens,
        })
        .collect()
}

pub struct IncidentUi {
    pub id: i64,
    pub pipeline: String,
    pub proc: String,
    pub err_code: String,
    pub status: String,
    pub triage: String,
}

pub fn load_incidents() -> Vec<IncidentUi> {
    let conn = db::open();
    incident::list_incidents_conn(&conn, None)
        .into_iter()
        .map(|i| IncidentUi {
            id: i.id,
            pipeline: i.pipeline,
            proc: i.proc_name,
            err_code: i.err_code,
            status: i.status,
            triage: i.triage,
        })
        .collect()
}

// ── 蓝图（DSL → DAG）──

pub struct BlueprintNode {
    pub name: String,
    pub layer: usize,
    pub verbs: Vec<String>,
    pub deliver: bool,
}

pub struct BlueprintEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    When,
    Needs,
    Trust,
    Foreach,
}

impl EdgeKind {
    pub fn glyph(&self) -> &'static str {
        match self {
            EdgeKind::When => "⋱when",
            EdgeKind::Needs => "─→",
            EdgeKind::Trust => "⚑trust",
            EdgeKind::Foreach => "⤳each",
        }
    }
}

pub struct Blueprint {
    pub name: String,
    pub nodes: Vec<BlueprintNode>,
    pub edges: Vec<BlueprintEdge>,
}

/// 从 when 条件文本提取 @ref 名（`@gate.err != "1"` → gate）。
fn refs_in_when(cond: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = cond.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > start {
                let name = cond[start..j].to_string();
                // 排除 @self 自引用
                if name != "self" && !out.contains(&name) {
                    out.push(name);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

pub fn load_blueprint(path: &str) -> Option<Blueprint> {
    // .hyper：走 HyperSpec 的 DAG 投影（stages：after/gated_by/deliver 齐备）
    if path.ends_with(".hyper") {
        let h = hyper::parse_hyper_file(path).ok()?;
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for s in &h.stages {
            nodes.push(BlueprintNode {
                name: s.name.clone(),
                layer: 0,
                verbs: vec![s.role.as_str().to_string()],
                deliver: h.deliver.as_deref() == Some(s.name.as_str()),
            });
            for a in &s.after {
                edges.push(BlueprintEdge {
                    from: a.clone(),
                    to: s.name.clone(),
                    kind: EdgeKind::Needs,
                });
            }
            if let Some(j) = &s.gated_by {
                edges.push(BlueprintEdge {
                    from: j.clone(),
                    to: s.name.clone(),
                    kind: EdgeKind::When,
                });
            }
        }
        return Some(finalize_blueprint(h.name, nodes, edges));
    }

    let pl = parser::parse_pipeline_file(path).ok()?;
    let mut nodes: Vec<BlueprintNode> = Vec::new();
    let mut edges: Vec<BlueprintEdge> = Vec::new();

    for p in &pl.procs {
        // impl 标签：impl 名（单 impl 糖下即动词名）
        let verbs: Vec<String> = p.plan.iter().map(|im| im.name.clone()).collect();
        nodes.push(BlueprintNode {
            name: p.name.clone(),
            layer: 0,
            verbs,
            deliver: p.deliver,
        });

        // 边三类：body @ref（Needs）/ when @ref（When）/ trust（Trust）/ foreach（Foreach）
        for im in &p.plan {
            for r in &im.refs {
                edges.push(BlueprintEdge {
                    from: r.clone(),
                    to: p.name.clone(),
                    kind: EdgeKind::Needs,
                });
            }
            if let Some(cond) = &im.when {
                for r in refs_in_when(cond) {
                    edges.push(BlueprintEdge {
                        from: r,
                        to: p.name.clone(),
                        kind: EdgeKind::When,
                    });
                }
            }
        }
        for r in &p.needs {
            edges.push(BlueprintEdge {
                from: r.clone(),
                to: p.name.clone(),
                kind: EdgeKind::Needs,
            });
        }
        for r in &p.trust_refs {
            edges.push(BlueprintEdge {
                from: r.clone(),
                to: p.name.clone(),
                kind: EdgeKind::Trust,
            });
        }
        if let Some(src) = &p.foreach {
            edges.push(BlueprintEdge {
                from: src.clone(),
                to: p.name.clone(),
                kind: EdgeKind::Foreach,
            });
        }
    }

    Some(finalize_blueprint(pl.name, nodes, edges))
}

/// 去重 + 拓扑分层（.pipeline 与 .hyper 两条装载路径共用）。
fn finalize_blueprint(
    name: String,
    mut nodes: Vec<BlueprintNode>,
    mut edges: Vec<BlueprintEdge>,
) -> Blueprint {
    // 去重（同一对 (from,to,kind) 只留一条）
    edges.dedup_by(|a, b| a.from == b.from && a.to == b.to && a.kind == b.kind);

    // 层计算：layer(n) = 1 + max(layer(上游))，无上游 = 0
    let mut layer_of: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for _ in 0..nodes.len() + 1 {
        let mut changed = false;
        for n in &nodes {
            let ups: Vec<&BlueprintEdge> = edges.iter().filter(|e| e.to == n.name).collect();
            let max_up = ups
                .iter()
                .map(|e| *layer_of.get(&e.from).unwrap_or(&0))
                .max();
            let want = max_up.map(|m| m + 1).unwrap_or(0);
            if layer_of.get(&n.name) != Some(&want) {
                layer_of.insert(n.name.clone(), want);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for n in &mut nodes {
        n.layer = *layer_of.get(&n.name).unwrap_or(&0);
    }
    Blueprint { name, nodes, edges }
}

pub fn load_iso(path: &str) -> Vec<String> {
    // 同构对照：similar JSON 报告逐段 + 本图 structure_key
    let mut lines = Vec::new();
    match hyper::similar_json(path, &[]) {
        Ok(rep) => {
            lines.push("── similar 报告 ──".to_string());
            // 单行 JSON 粗切：hits 数组项按逗号分段呈现
            lines.push(rep);
        }
        Err(e) => lines.push(format!("similar: {e}")),
    }
    if let Ok(pl) = parser::parse_pipeline_file(path) {
        lines.push(String::new());
        lines.push("── structure_key ──".to_string());
        let sig = hyper::struct_sig_from_pipeline(&pl);
        lines.push(sig.structure_key());
    }
    lines
}

// ── 渲染 ──

pub fn draw(app: &mut App, f: &mut Frame) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    // 顶栏：logo + tabs
    let tabs: Vec<&str> = vec![
        Tab::Status.title(),
        Tab::Data.title(),
        Tab::Blueprint.title(),
        Tab::Isomorph.title(),
    ];
    let titles: Vec<Span> = tabs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let sel = std::mem::discriminant(&app.tab)
                == std::mem::discriminant(&match i {
                    0 => Tab::Status,
                    1 => Tab::Data,
                    2 => Tab::Blueprint,
                    _ => Tab::Isomorph,
                });
            if sel {
                Span::styled(format!(" {t} "), Style::default().fg(GOLD).bold())
            } else {
                Span::styled(format!(" {t} "), Style::default().fg(DIM))
            }
        })
        .collect();
    let mut title_line = vec![
        Span::styled(" DUCTILE ", Style::default().fg(BG).bg(GOLD).bold()),
        Span::raw(" "),
        Span::styled("铸渠操作台", Style::default().fg(BLUE)),
        Span::raw("  │"),
    ];
    title_line.extend(titles);
    let header = Paragraph::new(Line::from(title_line));
    f.render_widget(header, chunks[0]);

    // 主体
    match app.tab {
        Tab::Status => draw_status(app, f, chunks[1]),
        Tab::Data => draw_data(app, f, chunks[1]),
        Tab::Blueprint => draw_blueprint(app, f, chunks[1]),
        Tab::Isomorph => draw_iso(app, f, chunks[1]),
    }

    // 底栏：键位提示
    let help = Paragraph::new(Line::from(vec![
        Span::styled(" 1-4", Style::default().fg(GOLD)),
        Span::styled("切换视图  ", Style::default().fg(DIM)),
        Span::styled("j/k", Style::default().fg(GOLD)),
        Span::styled("光标  ", Style::default().fg(DIM)),
        Span::styled("Enter", Style::default().fg(GOLD)),
        Span::styled("加载选中  ", Style::default().fg(DIM)),
        Span::styled("/", Style::default().fg(GOLD)),
        Span::styled("过滤库  ", Style::default().fg(DIM)),
        Span::styled("r", Style::default().fg(GOLD)),
        Span::styled("刷新  ", Style::default().fg(DIM)),
        Span::styled("q", Style::default().fg(GOLD)),
        Span::styled("退出", Style::default().fg(DIM)),
    ]));
    f.render_widget(help, chunks[2]);
}

fn panel(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BLUE))
        .style(Style::default().bg(PANEL).fg(TEXT))
        .title(Span::styled(
            format!(" {} ", title.into()),
            Style::default().fg(GOLD).bold(),
        ))
}

fn draw_status(app: &mut App, f: &mut Frame, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // 左：库计数
    let (pips, procs, runs, scripts) = app.counts;
    let left_lines = vec![
        Line::from(vec![
            Span::styled(" pipelines ", Style::default().fg(GOLD)),
            Span::raw(format!("{pips:>8}")),
        ]),
        Line::from(vec![
            Span::styled(" procs     ", Style::default().fg(GOLD)),
            Span::raw(format!("{procs:>8}")),
        ]),
        Line::from(vec![
            Span::styled(" runs      ", Style::default().fg(GOLD)),
            Span::raw(format!("{runs:>8}")),
        ]),
        Line::from(vec![
            Span::styled(" scripts   ", Style::default().fg(GOLD)),
            Span::raw(format!("{scripts:>8}")),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "── cognition 层 ──",
            Style::default().fg(BLUE),
        )),
        Line::from(vec![
            Span::styled(" l4 阶段   ", Style::default().fg(GOLD)),
            Span::styled(
                app.status.l4_phase.clone(),
                Style::default().fg(if app.status.l4_phase == "enforcing" {
                    CYAN
                } else {
                    DIM
                }),
            ),
        ]),
        Line::from(vec![
            Span::styled(" l4 一致率 ", Style::default().fg(GOLD)),
            Span::raw(
                app.status
                    .l4_agreement
                    .map(|v| format!("{v:.1}%"))
                    .unwrap_or_else(|| "—".into()),
            ),
        ]),
    ];
    f.render_widget(
        Paragraph::new(left_lines)
            .block(panel("库 / 认知层"))
            .wrap(Wrap { trim: false }),
        cols[0],
    );

    // 右：incidents / degraded / canary
    let inc_color = if app.status.incidents_open > 0 {
        RED
    } else {
        CYAN
    };
    let mut right_lines = vec![Line::from(vec![
        Span::styled(" incidents open ", Style::default().fg(GOLD)),
        Span::styled(
            format!("{}", app.status.incidents_open),
            Style::default().fg(inc_color),
        ),
        Span::styled(
            format!(" / {}", app.status.incidents_total),
            Style::default().fg(DIM),
        ),
    ])];
    if app.status.degraded.is_empty() {
        right_lines.push(Line::from(Span::styled(
            " degraded  无 — 全部管线健康",
            Style::default().fg(CYAN),
        )));
    } else {
        right_lines.push(Line::from(Span::styled(
            " degraded  ⚠ 降级中的管线：",
            Style::default().fg(RED),
        )));
        for d in &app.status.degraded {
            right_lines.push(Line::from(format!("   ◆ {d}")));
        }
    }
    right_lines.push(Line::from(""));
    // canary 通过率 gauge
    let rate = if app.status.canary_total > 0 {
        (app.status.canary_pass as f64 / app.status.canary_total as f64) * 100.0
    } else {
        0.0
    };
    let rate_line = Line::from(vec![
        Span::styled(" canary 通过率 ", Style::default().fg(GOLD)),
        Span::styled(
            if app.status.canary_total > 0 {
                format!(
                    "{:.1}% ({}/{})",
                    rate, app.status.canary_pass, app.status.canary_total
                )
            } else {
                "— 无记录".into()
            },
            Style::default().fg(if rate >= 75.0 {
                CYAN
            } else if rate >= 25.0 {
                GOLD
            } else {
                RED
            }),
        ),
    ]);
    right_lines.push(rate_line);
    f.render_widget(
        Paragraph::new(right_lines)
            .block(panel("哨兵 / 降级"))
            .wrap(Wrap { trim: false }),
        cols[1],
    );
}

fn draw_data(app: &mut App, f: &mut Frame, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(area);

    // 上：runs 浏览器
    let items: Vec<ListItem> = app
        .runs
        .iter()
        .map(|r| {
            let color = if r.status == "Ok" { CYAN } else { RED };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<24}", r.proc), Style::default().fg(TEXT)),
                Span::styled(format!("{:<10}", r.impl_), Style::default().fg(DIM)),
                Span::styled(format!("{:<5}", r.status), Style::default().fg(color)),
                Span::styled(format!("{:>6}ms", r.latency_ms), Style::default().fg(DIM)),
                Span::styled(format!(" {:>7}tk", r.tokens), Style::default().fg(DIM)),
                Span::styled(format!(" {}", r.at), Style::default().fg(DIM)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(panel("runs 最近执行（j/k 选择）"))
        .highlight_style(Style::default().bg(BLUE).fg(BG));
    f.render_stateful_widget(list, rows[0], &mut ratatui::widgets::ListState::default());

    // 下：incidents
    let inc_items: Vec<ListItem> = if app.incidents.is_empty() {
        vec![ListItem::new(Span::styled(
            "无 open incidents — 清白",
            Style::default().fg(CYAN),
        ))]
    } else {
        app.incidents
            .iter()
            .map(|i| {
                let color = if i.status == "open" { RED } else { DIM };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("#{:<4}", i.id), Style::default().fg(GOLD)),
                    Span::styled(format!("{:<20}", i.pipeline), Style::default().fg(TEXT)),
                    Span::styled(format!("{:<16}", i.proc), Style::default().fg(TEXT)),
                    Span::styled(format!("{:<12}", i.err_code), Style::default().fg(color)),
                    Span::styled(format!("{:<10}", i.status), Style::default().fg(color)),
                    Span::styled(i.triage.clone(), Style::default().fg(DIM)),
                ]))
            })
            .collect()
    };
    f.render_widget(List::new(inc_items).block(panel("incidents")), rows[1]);
}

/// 库选择器侧栏：db 注册表列表 + 光标高亮 + 过滤态提示。
/// 返回值 = 剩余区域（蓝图/同构主面板渲染处）。
fn draw_library(app: &App, f: &mut Frame, area: Rect) -> Rect {
    let outer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);

    let lib = app.library_filtered();
    let items: Vec<ListItem> = if lib.is_empty() {
        vec![ListItem::new(Span::styled(
            "（db 注册表为空——ductile import <dir>）",
            Style::default().fg(DIM),
        ))]
    } else {
        lib.iter()
            .enumerate()
            .map(|(i, (name, sf))| {
                let sel = i == app.lib_cursor;
                let style = if sel {
                    Style::default().bg(BLUE).fg(BG).bold()
                } else {
                    Style::default().fg(TEXT)
                };
                let dead = !std::path::Path::new(sf).exists();
                let mark = if dead { "✗ " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{mark}"), Style::default().fg(RED)),
                    Span::styled(name.clone(), style),
                ]))
            })
            .collect()
    };
    let title = if app.lib_filtering {
        format!("库 ({}项) 过滤: {}", lib.len(), app.lib_filter)
    } else {
        format!("库 ({}项) /过滤 Enter选", lib.len())
    };
    let list = List::new(items)
        .block(panel(title))
        .highlight_style(Style::default().bg(BLUE).fg(BG));
    f.render_widget(list, outer[0]);
    outer[1]
}

fn draw_blueprint(app: &mut App, f: &mut Frame, area: Rect) {
    let main = draw_library(app, f, area);
    let Some(bp) = &app.blueprint else {
        let msg = if app.sel.is_none() {
            "未选管线——左侧库列表 j/k 移动，Enter 加载蓝图\n（或启动时 ductile tui <path.pipeline> 直连）"
        } else {
            "解析失败——源文件路径失效或 DSL 语法错误（左侧 ✗ = 死路径）"
        };
        f.render_widget(
            Paragraph::new(msg)
                .block(panel("蓝图"))
                .style(Style::default().fg(DIM))
                .wrap(Wrap { trim: false }),
            main,
        );
        return;
    };

    // 分层布局：每层一行，层内等距。节点框宽度按层内数量自适应。
    let max_layer = bp.nodes.iter().map(|n| n.layer).max().unwrap_or(0);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            (0..=max_layer)
                .map(|_| Constraint::Length(3))
                .chain(std::iter::once(Constraint::Min(0)))
                .collect::<Vec<_>>(),
        )
        .split(main);

    let by_layer: std::collections::BTreeMap<usize, Vec<&BlueprintNode>> = {
        let mut m = std::collections::BTreeMap::new();
        for n in &bp.nodes {
            m.entry(n.layer).or_insert_with(Vec::new).push(n);
        }
        m
    };

    for (layer, nodes) in &by_layer {
        let row = rows[*layer];
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(
                (0..nodes.len())
                    .map(|_| Constraint::Ratio(1, nodes.len() as u32))
                    .collect::<Vec<_>>(),
            )
            .split(row);
        for (i, n) in nodes.iter().enumerate() {
            let title = if n.deliver {
                format!("◆ {}", n.name)
            } else {
                n.name.clone()
            };
            let verbs = n.verbs.join(",");
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if n.deliver { GOLD } else { BLUE }))
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(if n.deliver { GOLD } else { TEXT })
                        .bold(),
                ))
                .style(Style::default().bg(PANEL));
            let p = Paragraph::new(Span::styled(verbs, Style::default().fg(DIM)))
                .block(block)
                .wrap(Wrap { trim: true });
            f.render_widget(p, cols[i]);
        }
    }

    // 底部：边列表（TUI 里画斜线不现实，边按类型符号呈现）
    let edge_area = rows[max_layer + 1];
    let mut edge_lines: Vec<Line> = vec![Line::from(Span::styled(
        format!(
            " {} 条边（needs ─→ / when ⋱ / trust ⚑ / each ⤳）",
            bp.edges.len()
        ),
        Style::default().fg(BLUE),
    ))];
    for e in bp.edges.iter().take(60) {
        edge_lines.push(Line::from(format!(
            "  {} {} {}",
            e.from,
            e.kind.glyph(),
            e.to
        )));
    }
    f.render_widget(
        Paragraph::new(edge_lines)
            .block(panel(format!("{} · 边", bp.name)))
            .wrap(Wrap { trim: false }),
        edge_area,
    );
}

fn draw_iso(app: &mut App, f: &mut Frame, area: Rect) {
    let main = draw_library(app, f, area);
    // 惰性加载：首次选中目标才扫 similar（全库注册表扫描贵且 noisy）
    if app.iso_lines.is_empty() && app.sel.is_some() && !app.iso_loaded {
        let (_, sf) = app.sel.clone().unwrap();
        app.iso_lines = super::views::load_iso(&sf);
        app.iso_loaded = true;
    }
    if app.iso_lines.is_empty() {
        let msg = if app.sel.is_none() {
            "未选管线——左侧库列表 j/k 移动，Enter 选中后自动加载同构对照"
        } else {
            "无同构数据"
        };
        f.render_widget(
            Paragraph::new(msg)
                .block(panel("同构"))
                .style(Style::default().fg(DIM)),
            main,
        );
        return;
    }
    let items: Vec<ListItem> = app
        .iso_lines
        .iter()
        .map(|l| {
            let style = if l.starts_with("──") {
                Style::default().fg(BLUE).bold()
            } else {
                Style::default().fg(TEXT)
            };
            ListItem::new(Span::styled(l.clone(), style))
        })
        .collect();
    f.render_widget(
        List::new(items).block(panel("同构对照 · similar + structure_key")),
        main,
    );
}
