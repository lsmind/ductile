//! TUI 应用状态机——事件循环、Tab 切换、数据装载。

use super::views;
use crate::L0_physical::db;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, TerminalOptions, Viewport};

pub enum Tab {
    Status,
    Data,
    Blueprint,
    Isomorph,
}

impl Tab {
    pub fn title(&self) -> &'static str {
        match self {
            Tab::Status => "① STATUS 状态",
            Tab::Data => "② DATA 日志/库",
            Tab::Blueprint => "③ BLUEPRINT 蓝图",
            Tab::Isomorph => "④ ISOMORPH 同构",
        }
    }
    pub fn next(&self) -> Tab {
        match self {
            Tab::Status => Tab::Data,
            Tab::Data => Tab::Blueprint,
            Tab::Blueprint => Tab::Isomorph,
            Tab::Isomorph => Tab::Status,
        }
    }
    pub fn prev(&self) -> Tab {
        match self {
            Tab::Status => Tab::Isomorph,
            Tab::Data => Tab::Status,
            Tab::Blueprint => Tab::Data,
            Tab::Isomorph => Tab::Blueprint,
        }
    }
}

pub struct App {
    pub tab: Tab,
    /// 蓝图/同构视图的目标 .pipeline 路径（CLI 传入或 DATA 视图选中）。
    pub path: Option<String>,
    pub status: views::StatusSnapshot,
    pub runs: Vec<views::RunRowUi>,
    pub incidents: Vec<views::IncidentUi>,
    pub selected_run: usize,
    pub selected_incident: usize,
    pub blueprint: Option<views::Blueprint>,
    pub iso_lines: Vec<String>,
    /// iso 是否已加载过（防止空结果反复触发惰性重扫）。
    pub iso_loaded: bool,
    pub quit: bool,
    /// 库计数（pipelines/procs/runs/scripts）。
    pub counts: (i64, i64, i64, i64),
}

impl App {
    pub fn new(path: Option<String>) -> Result<Self, String> {
        let counts = db::db_stats();
        let status = views::load_status();
        let runs = views::load_runs(200);
        let incidents = views::load_incidents();
        let mut app = App {
            tab: Tab::Status,
            path: path.clone(),
            status,
            runs,
            incidents,
            selected_run: 0,
            selected_incident: 0,
            blueprint: None,
            iso_lines: Vec::new(),
            iso_loaded: false,
            quit: false,
            counts,
        };
        // 蓝图即时加载（本地 parse，便宜）；similar 同构惰性——
        // 扫全库注册表要几百 ms 且向 stderr 吐 dead-entry 噪声，
        // 进 ISOMORPH 视图才加载（首次 draw_iso 触发）。
        if let Some(p) = &path {
            app.blueprint = views::load_blueprint(p);
        }
        Ok(app)
    }

    pub fn load_blueprint(&mut self, path: &str) {
        self.blueprint = views::load_blueprint(path);
        self.iso_loaded = false; // 换目标后允许重扫 similar
        self.iso_lines.clear();
        self.path = Some(path.to_string());
    }

    pub fn run(&mut self) -> Result<(), String> {
        enable_raw_mode().map_err(|e| e.to_string())?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(stdout, EnterAlternateScreen).map_err(|e| e.to_string())?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )
        .map_err(|e| e.to_string())?;

        let res = self.event_loop(&mut terminal);

        disable_raw_mode().ok();
        crossterm::execute!(std::io::stdout(), LeaveAlternateScreen).ok();
        res
    }

    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<(), String> {
        while !self.quit {
            terminal
                .draw(|f| views::draw(self, f))
                .map_err(|e| e.to_string())?;
            if event::poll(std::time::Duration::from_millis(100)).map_err(|e| e.to_string())? {
                if let Event::Key(key) = event::read().map_err(|e| e.to_string())? {
                    if key.kind == KeyEventKind::Press {
                        self.on_key(key.code);
                    }
                }
            }
        }
        Ok(())
    }

    fn on_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => self.tab = self.tab.next(),
            KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => self.tab = self.tab.prev(),
            KeyCode::Char('1') => self.tab = Tab::Status,
            KeyCode::Char('2') => self.tab = Tab::Data,
            KeyCode::Char('3') => self.tab = Tab::Blueprint,
            KeyCode::Char('4') => self.tab = Tab::Isomorph,
            KeyCode::Up | KeyCode::Char('k') => self.cursor_up(),
            KeyCode::Down | KeyCode::Char('j') => self.cursor_down(),
            KeyCode::Char('r') => self.refresh(),
            _ => {}
        }
    }

    fn cursor_up(&mut self) {
        match self.tab {
            Tab::Data => {
                if self.selected_run > 0 {
                    self.selected_run -= 1
                }
            }
            Tab::Isomorph => {
                // 同构视图滚屏（沿 runs 光标复用 selected_run 计数）
                if self.selected_run > 0 {
                    self.selected_run -= 1
                }
            }
            _ => {}
        }
    }

    fn cursor_down(&mut self) {
        match self.tab {
            Tab::Data => {
                if self.selected_run + 1 < self.runs.len() {
                    self.selected_run += 1
                }
            }
            Tab::Isomorph => {
                self.selected_run += 1;
            }
            _ => {}
        }
    }

    fn refresh(&mut self) {
        self.counts = db::db_stats();
        self.status = views::load_status();
        self.runs = views::load_runs(200);
        self.incidents = views::load_incidents();
        if let Some(p) = self.path.clone() {
            self.load_blueprint(&p);
        }
    }
}
