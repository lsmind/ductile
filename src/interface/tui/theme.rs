//! TUI 主题——蓝金暗色，花哨但克制。

use ratatui::style::Color;

pub const BG: Color = Color::Rgb(10, 14, 26); // 深空底
pub const PANEL: Color = Color::Rgb(16, 22, 38); // 面板底
pub const GOLD: Color = Color::Rgb(255, 215, 122); // 金——标题/高亮/选中
pub const BLUE: Color = Color::Rgb(122, 162, 247); // 蓝——链接/边/次级强调
pub const CYAN: Color = Color::Rgb(86, 226, 228); // 青——成功/ok
pub const RED: Color = Color::Rgb(255, 120, 120); // 红——错误/incident open
pub const DIM: Color = Color::Rgb(95, 105, 135); // 暗灰——次要文本
pub const TEXT: Color = Color::Rgb(200, 210, 230); // 正文

pub const TITLE_GLYPH: &str = "◆";
