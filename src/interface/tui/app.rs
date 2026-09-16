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
    /// 当前选中管线的（name, source_file）——一律来自 db 注册表选择。
    pub sel: Option<(String, String)>,
    /// 库选择器列表（db 注册表全量）+ 光标。
    pub library: Vec<(String, String)>,
    pub lib_cursor: usize,
    /// 库选择器过滤词（/ 进入输入，Enter 选中）。
    pub lib_filter: String,
    pub lib_filtering: bool,
    pub status: views::StatusSnapshot,
    pub runs: Vec<views::RunRowUi>,
    pub incidents: Vec<views::IncidentUi>,
    pub selected_run: usize,
    pub blueprint: Option<views::Blueprint>,
    pub iso_lines: Vec<String>,
    /// iso 是否已加载过（防止空结果反复触发惰性重扫）。
    pub iso_loaded: bool,
    pub quit: bool,
    /// 库计数（pipelines/procs/runs/scripts）。
    pub counts: (i64, i64, i64, i64),
}

impl App {
    pub fn new(direct_path: Option<String>) -> Result<Self, String> {
        let conn = db::open();
        let counts = db::db_stats();
        let status = views::load_status();
        let runs = views::load_runs(200);
        let incidents = views::load_incidents();
        let library = db::registered_graph_files(&conn);
        // CLI 直连路径：在注册表里找同路径的条目对齐；找不到就以
        // (文件名, 路径) 直接选中（未注册文件也能看蓝图）。
        let sel = direct_path.map(|p| {
            let name = library
                .iter()
                .find(|(_, sf)| sf == &p)
                .map(|(n, _)| n.clone())
                .unwrap_or_else(|| {
                    std::path::Path::new(&p)
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.clone())
                });
            (name, p)
        });
        let mut app = App {
            tab: Tab::Status,
            sel,
            library,
            lib_cursor: 0,
            lib_filter: String::new(),
            lib_filtering: false,
            status,
            runs,
            incidents,
            selected_run: 0,
            blueprint: None,
            iso_lines: Vec::new(),
            iso_loaded: false,
            quit: false,
            counts,
        };
        // 有直连目标才预载蓝图；similar 同构保持惰性（扫全库贵且 noisy）。
        if let Some((_, sf)) = &app.sel {
            app.blueprint = views::load_blueprint(sf);
        }
        Ok(app)
    }

    pub fn load_pipeline(&mut self, name: &str, source_file: &str) {
        self.blueprint = views::load_blueprint(source_file);
        self.iso_loaded = false; // 换目标后允许重扫 similar
        self.iso_lines.clear();
        self.sel = Some((name.to_string(), source_file.to_string()));
    }

    /// 过滤后的库列表（filter 空则全量）。
    pub fn library_filtered(&self) -> Vec<(String, String)> {
        if self.lib_filter.is_empty() {
            return self.library.clone();
        }
        let f = self.lib_filter.to_lowercase();
        self.library
            .iter()
            .filter(|(n, sf)| n.to_lowercase().contains(&f) || sf.to_lowercase().contains(&f))
            .cloned()
            .collect()
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
        // 过滤输入模式：字符进过滤词，Enter/Esc 退出输入态（Enter 时若光标在列表上则选中）
        if self.lib_filtering {
            match code {
                KeyCode::Char(c) => self.lib_filter.push(c),
                KeyCode::Backspace => {
                    self.lib_cursor = 0;
                    self.lib_filter.pop();
                }
                KeyCode::Esc => {
                    self.lib_filtering = false;
                    self.lib_filter.clear();
                }
                KeyCode::Enter => {
                    let lib = self.library_filtered();
                    if let Some((name, sf)) = lib.get(self.lib_cursor).cloned() {
                        self.load_pipeline(&name, &sf);
                    }
                    self.lib_filtering = false;
                }
                // 注：过滤态按 j/k 会进过滤词（上面 Char 分支先吃掉），
                // 想移动光标用方向键
                KeyCode::Up => {
                    if self.lib_cursor > 0 {
                        self.lib_cursor -= 1;
                    }
                }
                KeyCode::Down => {
                    let n = self.library_filtered().len();
                    if self.lib_cursor + 1 < n {
                        self.lib_cursor += 1;
                    }
                }
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => self.tab = self.tab.next(),
            KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => self.tab = self.tab.prev(),
            KeyCode::Char('1') => self.tab = Tab::Status,
            KeyCode::Char('2') => self.tab = Tab::Data,
            KeyCode::Char('3') => self.tab = Tab::Blueprint,
            KeyCode::Char('4') => self.tab = Tab::Isomorph,
            KeyCode::Char('/') => {
                // 进入过滤输入（只在 BLUEPRINT/ISOMORPH 有意义）
                self.lib_filtering = true;
            }
            KeyCode::Enter => {
                // 库选择器：回车加载光标处管线（BLUEPRINT/ISOMORPH 视图）
                if matches!(self.tab, Tab::Blueprint | Tab::Isomorph) {
                    let lib = self.library_filtered();
                    if let Some((name, sf)) = lib.get(self.lib_cursor).cloned() {
                        self.load_pipeline(&name, &sf);
                    }
                }
            }
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
            Tab::Blueprint | Tab::Isomorph => {
                if self.lib_cursor > 0 {
                    self.lib_cursor -= 1
                }
            }
            Tab::Status => {}
        }
    }

    fn cursor_down(&mut self) {
        match self.tab {
            Tab::Data => {
                if self.selected_run + 1 < self.runs.len() {
                    self.selected_run += 1
                }
            }
            Tab::Blueprint | Tab::Isomorph => {
                let n = self.library_filtered().len();
                if self.lib_cursor + 1 < n {
                    self.lib_cursor += 1;
                }
            }
            Tab::Status => {}
        }
    }

    fn refresh(&mut self) {
        let conn = db::open();
        self.counts = db::db_stats();
        self.library = db::registered_graph_files(&conn);
        drop(conn);
        self.status = views::load_status();
        self.runs = views::load_runs(200);
        self.incidents = views::load_incidents();
        if let Some((name, sf)) = self.sel.clone() {
            self.load_pipeline(&name, &sf);
        }
    }
}
