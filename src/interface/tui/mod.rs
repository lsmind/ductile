//! ductile TUI — 花哨终端操作台（v0.20 蓝图）。
//!
//! 四视图（Tab 切换）：
//!   1 STATUS  工作状态概览——库计数/cognition 层旗标（incidents/canary/l4/degraded）
//!   2 DATA    日志与数据库——runs 浏览器 + incidents 列表 + db stats
//!   3 BLUEPRINT DSL→DAG 节点蓝图——procs 分层布局，when/needs/trust 边
//!   4 ISOMORPH 同构对照——hyper similar 报告 + structure_key
//!
//! 纯读侧：TUI 不写库不跑管线，只消费 db 与 hyper 的只读查询。
//! 视觉：蓝金暗色（#0a0e1a 底 / #ffd77a 金 / #7aa2f7 蓝）。

mod app;
mod theme;
mod views;

use crate::core::dslresult::extract_field;

/// 便捷：从 §§FIELDS§§ 文本提字段（views 共用）。
pub fn field_of(text: &str, key: &str) -> Option<String> {
    extract_field(key, text)
}

pub fn run_tui(path: Option<String>) -> Result<(), String> {
    let mut app = app::App::new(path)?;
    app.run()
}
