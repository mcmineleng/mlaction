use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::Arc;
use std::sync::OnceLock;

use brush_core::builtins::{self, Command};
use brush_core::env::{EnvironmentLookup, EnvironmentScope};
use brush_core::openfiles::OpenFile;
use brush_core::variables::ShellValueLiteral;
use brush_core::{
    CreateOptions, ExecutionContext, ExecutionParameters, ExecutionResult,
    Shell as BrushShell, ShellFd,
};
use clap::Parser;
use serde::Deserialize;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self as cterm, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color as TuiColor, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PKG_NAME: &str = env!("CARGO_PKG_NAME");

mod color {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BLUE: &str = "\x1b[34m";
    pub const CYAN: &str = "\x1b[36m";
    pub const DIM: &str = "\x1b[2m";
}

fn default_true() -> bool { true }

// ============== max_depth 设置类型 ==============
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum MaxDepthSetting {
    Bool(bool),
    Num(usize),
}

impl Default for MaxDepthSetting {
    fn default() -> Self { MaxDepthSetting::Num(64) }
}

fn default_max_depth() -> MaxDepthSetting { MaxDepthSetting::Num(64) }

// ============== 配置结构 ==============
#[derive(Debug, Deserialize, Clone, Default)]
struct TaskSettings {
    #[serde(default)]
    actioninfo: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
struct GlobalSettings {
    #[serde(default = "default_true")]
    actioninfo: bool,
    #[serde(default = "default_max_depth")]
    max_depth: MaxDepthSetting,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            actioninfo: true,
            max_depth: MaxDepthSetting::Num(64),
        }
    }
}

#[derive(Debug, Deserialize)]
struct BuildFile {
    #[serde(rename = "task", default)]
    tasks: Vec<Task>,
    #[serde(rename = "input", default)]
    inputs: Vec<Input>,
    #[serde(default)]
    settings: GlobalSettings,
}

#[derive(Debug, Deserialize, Clone)]
struct Input {
    #[serde(rename = "type")]
    input_type: String,
    name: String,
    key: String,
    #[serde(default)]
    require: bool,
    #[serde(default)]
    choose: Vec<Choose>,
}

#[derive(Debug, Deserialize, Clone)]
struct Choose {
    name: String,
    key: String,
}

#[derive(Debug, Deserialize, Clone)]
struct Task {
    name: String,
    value: String,
    #[serde(default)]
    run_cond: Option<String>,
    #[serde(default)]
    require_success: bool,
    #[serde(default)]
    settings: TaskSettings,
    #[serde(rename = "step", default)]
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize, Clone)]
struct Step {
    name: String,
    #[serde(default)]
    require_success: bool,
    #[serde(rename = "action", default)]
    actions: Vec<Action>,
}

#[derive(Debug, Deserialize, Clone)]
struct Action {
    #[serde(default)]
    #[allow(dead_code)]
    require_success: bool,
    run: String,
}

// ============== 输入的收集值 ==============
#[derive(Debug, Clone)]
enum InputValue {
    String(String),
    Int(String),
    Bool(Option<bool>),
    OnlyChose(Option<usize>),
    ManyChose(Vec<usize>),
}

// ============== 日志 ==============
fn log_line(show: bool, level: usize, label: &str, name: &str, status: String) {
    if !show {
        return;
    }
    let indent = "    ".repeat(level);
    let c = match label {
        "TASK" => color::CYAN,
        "STEP" => color::BLUE,
        _ => color::RESET,
    };
    let mut out = std::io::stdout().lock();
    let _ = write!(
        out,
        "\r\x1b[K{indent}{BOLD}{c}{label}{RESET} : {name} {s}\r\n",
        indent = indent,
        BOLD = color::BOLD,
        c = c,
        label = label,
        RESET = color::RESET,
        name = name,
        s = status,
    );
    let _ = out.flush();
}

fn s_start() -> String { format!("{}[开始]{}", color::YELLOW, color::RESET) }
fn s_ok() -> String { format!("{}[成功]{}", color::GREEN, color::RESET) }
fn s_fail(c: i32) -> String { format!("{}[失败({})]{}", color::RED, c, color::RESET) }
fn s_interrupt() -> String { format!("{}[中断(Ctrl+C)]{}", color::RED, color::RESET) }
fn s_skip(reason: &str) -> String {
    format!("{}[跳过({})]{}", color::YELLOW, reason, color::RESET)
}

fn err_line(msg: String) {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\r\x1b[K{msg}\r\n");
    let _ = out.flush();
}

// ============== 终端 raw mode guard ==============
struct RawModeGuard {
    original: Option<libc::termios>,
}

impl RawModeGuard {
    fn enter() -> Self {
        if !std::io::stdin().is_terminal() {
            return Self { original: None };
        }
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return Self { original: None };
            }
            let mut raw = original;
            libc::cfmakeraw(&mut raw);
            raw.c_lflag |= libc::ISIG;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return Self { original: None };
            }
            Self { original: Some(original) }
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Some(ref orig) = self.original {
            unsafe {
                let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, orig);
            }
        }
    }
}

// ============== TUI：输入收集 ==============
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Inputs,
    Cancel,
    Confirm,
    Fill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Main,
    Edit,
}

#[derive(Debug, Clone, Default)]
struct EditState {
    text: String,
    cursor: usize,
}

#[derive(Debug)]
struct InputApp {
    defs: Vec<Input>,
    values: Vec<Option<InputValue>>,
    active: usize,
    focus: Focus,
    screen: Screen,
    edit: EditState,
    message: Option<String>,
}

enum InputAction {
    None,
    Submit,
    Cancel,
}

impl InputApp {
    fn new(defs: Vec<Input>) -> Self {
        let n = defs.len();
        let mut values: Vec<Option<InputValue>> = Vec::with_capacity(n);
        for d in &defs {
            let v = match d.input_type.as_str() {
                "string" => Some(InputValue::String(String::new())),
                "int" => Some(InputValue::Int(String::new())),
                "bool" => Some(InputValue::Bool(None)),
                "only_chose" => Some(InputValue::OnlyChose(None)),
                "many_chose" => Some(InputValue::ManyChose(Vec::new())),
                _ => None,
            };
            values.push(v);
        }
        Self {
            defs,
            values,
            active: 0,
            focus: Focus::Inputs,
            screen: Screen::Main,
            edit: EditState::default(),
            message: None,
        }
    }

    fn validate(&self) -> Option<String> {
        for (i, def) in self.defs.iter().enumerate() {
            if !def.require {
                continue;
            }
            let ok = match &self.values[i] {
                Some(InputValue::String(s)) => !s.is_empty(),
                Some(InputValue::Int(s)) => !s.is_empty(),
                Some(InputValue::Bool(Some(_))) => true,
                Some(InputValue::OnlyChose(Some(_))) => true,
                Some(InputValue::ManyChose(v)) => !v.is_empty(),
                _ => false,
            };
            if !ok {
                return Some(format!("\"{}\" 是必填项", def.name));
            }
        }
        None
    }

    fn into_map(self) -> HashMap<String, InputValue> {
        let mut m = HashMap::new();
        for (i, def) in self.defs.into_iter().enumerate() {
            if let Some(v) = self.values[i].clone() {
                m.insert(def.key, v);
            }
        }
        m
    }

    fn enter_edit(&mut self) {
        let def = &self.defs[self.active];
        match def.input_type.as_str() {
            "string" => {
                self.edit.text = match &self.values[self.active] {
                    Some(InputValue::String(s)) => s.clone(),
                    _ => String::new(),
                };
            }
            "int" => {
                self.edit.text = match &self.values[self.active] {
                    Some(InputValue::Int(s)) => s.clone(),
                    _ => String::new(),
                };
            }
            "bool" => {
                self.edit.cursor = match &self.values[self.active] {
                    Some(InputValue::Bool(Some(true))) => 0,
                    Some(InputValue::Bool(Some(false))) => 1,
                    _ => 0,
                };
            }
            "only_chose" => {
                self.edit.cursor = match &self.values[self.active] {
                    Some(InputValue::OnlyChose(Some(i))) => *i,
                    _ => 0,
                };
            }
            "many_chose" => {
                self.edit.cursor = 0;
            }
            _ => {}
        }
        self.screen = Screen::Edit;
    }

    fn handle_key(&mut self, k: KeyEvent) -> InputAction {
        // Ctrl+C 在任何界面都视为取消
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return InputAction::Cancel;
        }
        match self.screen {
            Screen::Main => self.handle_main(k),
            Screen::Edit => self.handle_edit(k),
        }
    }

    fn handle_main(&mut self, k: KeyEvent) -> InputAction {
        self.message = None;
        match k.code {
            KeyCode::Esc => return InputAction::Cancel,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Inputs => Focus::Cancel,
                    Focus::Cancel => Focus::Confirm,
                    Focus::Confirm => Focus::Fill,
                    Focus::Fill => Focus::Inputs,
                };
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Inputs => Focus::Fill,
                    Focus::Cancel => Focus::Inputs,
                    Focus::Confirm => Focus::Cancel,
                    Focus::Fill => Focus::Confirm,
                };
            }
            KeyCode::Up => {
                if self.focus == Focus::Inputs && self.active > 0 {
                    self.active -= 1;
                }
            }
            KeyCode::Down => {
                if self.focus == Focus::Inputs && self.active + 1 < self.defs.len() {
                    self.active += 1;
                }
            }
            KeyCode::Enter => match self.focus {
                Focus::Inputs => self.enter_edit(),
                Focus::Fill => self.focus = Focus::Inputs,
                Focus::Cancel => return InputAction::Cancel,
                Focus::Confirm => return InputAction::Submit,
            },
            _ => {}
        }
        InputAction::None
    }

    fn handle_edit(&mut self, k: KeyEvent) -> InputAction {
        let t = self.defs[self.active].input_type.clone();
        match t.as_str() {
            "string" | "int" => match k.code {
                KeyCode::Enter => {
                    let val = if t == "int" {
                        InputValue::Int(self.edit.text.clone())
                    } else {
                        InputValue::String(self.edit.text.clone())
                    };
                    self.values[self.active] = Some(val);
                    self.screen = Screen::Main;
                }
                KeyCode::Esc => {
                    self.screen = Screen::Main;
                }
                KeyCode::Backspace => {
                    self.edit.text.pop();
                }
                KeyCode::Char(c) => {
                    if t == "int" && !c.is_ascii_digit() {
                        return InputAction::None;
                    }
                    self.edit.text.push(c);
                }
                _ => {}
            },
            "bool" => match k.code {
                KeyCode::Up => self.edit.cursor = 0,
                KeyCode::Down => self.edit.cursor = 1,
                KeyCode::Enter => {
                    self.values[self.active] =
                        Some(InputValue::Bool(Some(self.edit.cursor == 0)));
                    self.screen = Screen::Main;
                }
                KeyCode::Esc => {
                    self.screen = Screen::Main;
                }
                _ => {}
            },
            "only_chose" => {
                let n = self.defs[self.active].choose.len();
                match k.code {
                    KeyCode::Up => {
                        if self.edit.cursor > 0 {
                            self.edit.cursor -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if self.edit.cursor + 1 < n {
                            self.edit.cursor += 1;
                        }
                    }
                    KeyCode::Enter => {
                        self.values[self.active] =
                            Some(InputValue::OnlyChose(Some(self.edit.cursor)));
                        self.screen = Screen::Main;
                    }
                    KeyCode::Esc => {
                        self.screen = Screen::Main;
                    }
                    _ => {}
                }
            }
            "many_chose" => {
                let n = self.defs[self.active].choose.len();
                match k.code {
                    KeyCode::Up => {
                        if self.edit.cursor > 0 {
                            self.edit.cursor -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if self.edit.cursor + 1 < n {
                            self.edit.cursor += 1;
                        }
                    }
                    KeyCode::Char(' ') => {
                        let mut v = match &self.values[self.active] {
                            Some(InputValue::ManyChose(v)) => v.clone(),
                            _ => Vec::new(),
                        };
                        if let Some(pos) = v.iter().position(|&x| x == self.edit.cursor) {
                            v.remove(pos);
                        } else {
                            v.push(self.edit.cursor);
                        }
                        v.sort();
                        self.values[self.active] = Some(InputValue::ManyChose(v));
                    }
                    KeyCode::Enter => {
                        self.screen = Screen::Main;
                    }
                    KeyCode::Esc => {
                        self.screen = Screen::Main;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        InputAction::None
    }

    fn render(&self, f: &mut Frame) {
        match self.screen {
            Screen::Main => self.render_main(f),
            Screen::Edit => self.render_edit(f),
        }
    }

    fn render_main(&self, f: &mut Frame) {
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(area);

        // 输入项列表
        let items: Vec<ListItem> = self
            .defs
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let marker = if self.focus == Focus::Inputs && i == self.active {
                    "> "
                } else {
                    "  "
                };
                let require_mark = if d.require { " *" } else { "" };
                ListItem::new(Line::from(format!(
                    "{}[{}] {}{}",
                    marker, d.input_type, d.name, require_mark
                )))
            })
            .collect();

        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" 请填写配置（回车进入编辑） "),
        );
        f.render_widget(list, chunks[0]);

        // 消息行
        if let Some(msg) = &self.message {
            let p = Paragraph::new(msg.as_str())
                .style(Style::default().fg(TuiColor::Red))
                .alignment(Alignment::Center);
            f.render_widget(p, chunks[1]);
        }

        // 按钮行
        let focused_idx = match self.focus {
            Focus::Cancel => Some(0),
            Focus::Confirm => Some(1),
            Focus::Fill => Some(2),
            _ => None,
        };
        let labels = [" 取 消 ", " 确 认 ", " 填 写 "];
        let mut spans: Vec<Span> = Vec::new();
        for (i, l) in labels.iter().enumerate() {
            let style = if Some(i) == focused_idx {
                Style::default()
                    .bg(TuiColor::Cyan)
                    .fg(TuiColor::Black)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TuiColor::Gray)
            };
            spans.push(Span::styled(l.to_string(), style));
            spans.push(Span::raw("    "));
        }
        let p = Paragraph::new(Line::from(spans)).alignment(Alignment::Center);
        f.render_widget(p, chunks[2]);
    }

    fn render_edit(&self, f: &mut Frame) {
        let def = &self.defs[self.active];
        let area = f.area();
        let title = format!(" {} ({}) ", def.name, def.input_type);

        let text: Vec<Line> = match def.input_type.as_str() {
            "string" | "int" => {
                vec![
                    Line::from(""),
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(self.edit.text.clone(), Style::default().fg(TuiColor::Cyan)),
                        Span::styled("█", Style::default().fg(TuiColor::Cyan)),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(
                        "  Enter 确认 / Esc 返回",
                        Style::default().fg(TuiColor::DarkGray),
                    )),
                ]
            }
            "bool" => {
                let mut lines = vec![Line::from("")];
                for (i, label) in ["true", "false"].iter().enumerate() {
                    let marker = if self.edit.cursor == i { "> " } else { "  " };
                    let style = if self.edit.cursor == i {
                        Style::default().fg(TuiColor::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    lines.push(Line::from(Span::styled(
                        format!("  {}{}", marker, label),
                        style,
                    )));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  ↑↓ 选择 / Enter 确认 / Esc 返回",
                    Style::default().fg(TuiColor::DarkGray),
                )));
                lines
            }
            "only_chose" => {
                let mut lines = vec![Line::from("")];
                let sel = match &self.values[self.active] {
                    Some(InputValue::OnlyChose(Some(i))) => Some(*i),
                    _ => None,
                };
                for (i, c) in def.choose.iter().enumerate() {
                    let marker = if self.edit.cursor == i { "> " } else { "  " };
                    let sel_mark = if Some(i) == sel { "(*)" } else { "( )" };
                    let style = if self.edit.cursor == i {
                        Style::default().fg(TuiColor::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    lines.push(Line::from(Span::styled(
                        format!("  {}{} {} [{}]", marker, sel_mark, c.name, c.key),
                        style,
                    )));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  ↑↓ 选择 / Enter 确认 / Esc 返回",
                    Style::default().fg(TuiColor::DarkGray),
                )));
                lines
            }
            "many_chose" => {
                let mut lines = vec![Line::from("")];
                let selected = match &self.values[self.active] {
                    Some(InputValue::ManyChose(v)) => v.clone(),
                    _ => Vec::new(),
                };
                for (i, c) in def.choose.iter().enumerate() {
                    let marker = if self.edit.cursor == i { "> " } else { "  " };
                    let sel_mark = if selected.contains(&i) { "[x]" } else { "[ ]" };
                    let style = if self.edit.cursor == i {
                        Style::default().fg(TuiColor::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    lines.push(Line::from(Span::styled(
                        format!("  {}{} {} [{}]", marker, sel_mark, c.name, c.key),
                        style,
                    )));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  ↑↓ 移动 / 空格 切换 / Enter 确认 / Esc 返回",
                    Style::default().fg(TuiColor::DarkGray),
                )));
                lines
            }
            _ => vec![],
        };

        let p = Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(title));
        f.render_widget(p, area);
    }
}

fn collect_inputs_sync(defs: Vec<Input>) -> Result<HashMap<String, InputValue>, ()> {
    // 非终端环境：全部用默认值
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        let app = InputApp::new(defs);
        return Ok(app.into_map());
    }

    cterm::enable_raw_mode().map_err(|_| ())?;
    let mut stdout = std::io::stdout();
    if execute!(stdout, EnterAlternateScreen).is_err() {
        cterm::disable_raw_mode().ok();
        return Err(());
    }

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(_) => {
            cterm::disable_raw_mode().ok();
            return Err(());
        }
    };

    let mut app = InputApp::new(defs);

    let result: Result<HashMap<String, InputValue>, ()> = loop {
        if terminal.draw(|f| app.render(f)).is_err() {
            break Err(());
        }
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(_) => break Err(()),
        };
        if let Event::Key(k) = ev {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match app.handle_key(k) {
                InputAction::Submit => {
                    if let Some(err) = app.validate() {
                        app.message = Some(err);
                    } else {
                        break Ok(app.into_map());
                    }
                }
                InputAction::Cancel => break Err(()),
                InputAction::None => {}
            }
        }
    };

    // 恢复终端
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    cterm::disable_raw_mode().ok();

    result
}

// ============== Engine ==============
struct Engine {
    tasks: HashMap<String, Task>,
    task_order: Vec<String>,
    global_vars: HashMap<String, String>,
    global_actioninfo: bool,
    task_depth: usize,
    max_depth: Option<usize>,
    input_defs: HashMap<String, Input>,
    input_values: HashMap<String, InputValue>,
}

impl Engine {
    fn new(
        tasks: Vec<Task>,
        global_actioninfo: bool,
        max_depth: MaxDepthSetting,
        inputs: Vec<Input>,
        input_values: HashMap<String, InputValue>,
    ) -> Result<Self, String> {
        let max_depth = match max_depth {
            MaxDepthSetting::Num(n) => Some(n),
            MaxDepthSetting::Bool(false) => None,
            MaxDepthSetting::Bool(true) => {
                return Err(
                    "settings.max_depth 为布尔值时只能为 false（表示不限制递归）".to_string(),
                );
            }
        };

        let mut map = HashMap::new();
        let mut order = Vec::with_capacity(tasks.len());
        for t in tasks {
            let key = t.value.clone();
            if map.contains_key(&key) {
                return Err(format!("task value 重复: {}", key));
            }
            order.push(key.clone());
            map.insert(key, t);
        }

        let mut input_defs = HashMap::new();
        for inp in inputs {
            input_defs.insert(inp.key.clone(), inp);
        }

        Ok(Self {
            tasks: map,
            task_order: order,
            global_vars: HashMap::new(),
            global_actioninfo,
            task_depth: 0,
            max_depth,
            input_defs,
            input_values,
        })
    }
}

static ENGINE: OnceLock<Arc<tokio::sync::Mutex<Engine>>> = OnceLock::new();

fn get_engine() -> Arc<tokio::sync::Mutex<Engine>> {
    ENGINE.get().expect("Engine 未初始化").clone()
}

// ============== 内建命令：task ==============
#[derive(Parser, Debug)]
#[command(name = "task", about = "调用一个 task")]
struct TaskCommand {
    #[arg(required = true)]
    target: String,
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
}

impl Command for TaskCommand {
    type Error = brush_core::Error;

    async fn execute(
        &self,
        _context: ExecutionContext<'_>,
    ) -> Result<ExecutionResult, Self::Error> {
        let engine = get_engine();
        match execute_task(engine, &self.target, &self.args).await {
            Ok(()) => Ok(ExecutionResult::success()),
            Err(code) => Ok(ExecutionResult::new(code as u8)),
        }
    }
}

// ============== 内建命令：setallvar ==============
#[derive(Parser, Debug)]
#[command(name = "setallvar", about = "将变量设置到全局变量池（唯一写入入口）")]
struct SetallvarCommand {
    #[arg(required = true)]
    name: String,
    #[arg(required = true)]
    value: String,
}

impl Command for SetallvarCommand {
    type Error = brush_core::Error;

    async fn execute(
        &self,
        _context: ExecutionContext<'_>,
    ) -> Result<ExecutionResult, Self::Error> {
        let engine = get_engine();
        let mut e = engine.lock().await;
        e.global_vars.insert(self.name.clone(), self.value.clone());
        Ok(ExecutionResult::success())
    }
}

// ============== 内建命令：getallvar ==============
#[derive(Parser, Debug)]
#[command(name = "getallvar", about = "从全局变量池读取变量并设为当前 shell 变量")]
struct GetallvarCommand {
    #[arg(required = true)]
    name: String,
}

impl Command for GetallvarCommand {
    type Error = brush_core::Error;

    async fn execute(
        &self,
        context: ExecutionContext<'_>,
    ) -> Result<ExecutionResult, Self::Error> {
        let engine = get_engine();
        let e = engine.lock().await;
        if let Some(val) = e.global_vars.get(&self.name) {
            context
                .shell
                .env
                .update_or_add(
                    self.name.as_str(),
                    ShellValueLiteral::Scalar(val.clone()),
                    |_| Ok(()),
                    EnvironmentLookup::Anywhere,
                    EnvironmentScope::Global,
                )
                .map_err(brush_core::Error::from)?;
            Ok(ExecutionResult::success())
        } else {
            err_line(format!(
                "{}[错误] 全局变量池中不存在: {}{}",
                color::RED, self.name, color::RESET
            ));
            Ok(ExecutionResult::new(1))
        }
    }
}

// ============== 内建命令：listallvar ==============
#[derive(Parser, Debug)]
#[command(name = "listallvar", about = "列出全局变量池中的所有变量")]
struct ListallvarCommand {
    #[arg(short = 'n')]
    names_only: bool,
    #[arg(short = 'v')]
    values: bool,
}

impl Command for ListallvarCommand {
    type Error = brush_core::Error;

    async fn execute(
        &self,
        _context: ExecutionContext<'_>,
    ) -> Result<ExecutionResult, Self::Error> {
        let engine = get_engine();
        let e = engine.lock().await;
        if e.global_vars.is_empty() {
            return Ok(ExecutionResult::success());
        }
        if self.names_only && !self.values {
            for k in e.global_vars.keys() {
                err_line(k.clone());
            }
        } else if !self.names_only && self.values {
            let parts: Vec<String> = e
                .global_vars
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect();
            err_line(parts.join(" "));
        } else if self.names_only && self.values {
            for (k, v) in &e.global_vars {
                err_line(format!("{}={}", k, v));
            }
        } else {
            let keys: Vec<&str> = e.global_vars.keys().map(|s| s.as_str()).collect();
            err_line(keys.join(" "));
        }
        Ok(ExecutionResult::success())
    }
}

// ============== 内建命令：getinput ==============
#[derive(Parser, Debug)]
#[command(name = "getinput", about = "读取 TUI 收集的 input 值并写入 shell 变量")]
struct GetinputCommand {
    #[arg(required = true)]
    input_type: String,
    #[arg(required = true)]
    key: String,
    #[arg(required = true)]
    varname: String,
}

impl Command for GetinputCommand {
    type Error = brush_core::Error;

    async fn execute(
        &self,
        context: ExecutionContext<'_>,
    ) -> Result<ExecutionResult, Self::Error> {
        let engine = get_engine();
        let e = engine.lock().await;

        let def = match e.input_defs.get(&self.key) {
            Some(d) => d.clone(),
            None => {
                err_line(format!(
                    "{}[错误] getinput: 未知 input key: {}{}",
                    color::RED, self.key, color::RESET
                ));
                return Ok(ExecutionResult::new(1));
            }
        };
        if def.input_type != self.input_type {
            err_line(format!(
                "{}[错误] getinput: type 不匹配 (期望 {} 实际 {}){}",
                color::RED, def.input_type, self.input_type, color::RESET
            ));
            return Ok(ExecutionResult::new(1));
        }

        let val = e.input_values.get(&self.key).cloned();

        match self.input_type.as_str() {
            "string" => {
                let s = match val {
                    Some(InputValue::String(s)) => s,
                    _ => String::new(),
                };
                set_shell_var(&context, &self.varname, s)?;
            }
            "int" => {
                let s = match val {
                    Some(InputValue::Int(s)) => s,
                    _ => String::new(),
                };
                set_shell_var(&context, &self.varname, s)?;
            }
            "bool" => {
                let s = match val {
                    Some(InputValue::Bool(Some(true))) => "true".to_string(),
                    Some(InputValue::Bool(Some(false))) => "false".to_string(),
                    _ => String::new(),
                };
                set_shell_var(&context, &self.varname, s)?;
            }
            "only_chose" => {
                let s = match val {
                    Some(InputValue::OnlyChose(Some(i))) => def
                        .choose
                        .get(i)
                        .map(|c| c.key.clone())
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                set_shell_var(&context, &self.varname, s)?;
            }
            "many_chose" => {
                let arr: Vec<String> = match val {
                    Some(InputValue::ManyChose(v)) => {
                        v.iter().map(|i| i.to_string()).collect()
                    }
                    _ => Vec::new(),
                };
                set_shell_array(&context, &self.varname, arr)?;
            }
            _ => {}
        }

        Ok(ExecutionResult::success())
    }
}

fn set_shell_var(
    context: &ExecutionContext<'_>,
    name: &str,
    value: String,
) -> Result<(), brush_core::Error> {
    context
        .shell
        .env
        .update_or_add(
            name,
            ShellValueLiteral::Scalar(value),
            |_| Ok(()),
            EnvironmentLookup::Anywhere,
            EnvironmentScope::Global,
        )
        .map_err(brush_core::Error::from)
}

fn set_shell_array(
    context: &ExecutionContext<'_>,
    name: &str,
    items: Vec<String>,
) -> Result<(), brush_core::Error> {
    context
        .shell
        .env
        .update_or_add(
            name,
            ShellValueLiteral::Array(items),
            |_| Ok(()),
            EnvironmentLookup::Anywhere,
            EnvironmentScope::Global,
        )
        .map_err(brush_core::Error::from)
}

// ============== 创建绑定到 PTY 的 shell ==============
async fn create_shell_with_pty(
    args: &[String],
    pts: Option<Arc<pty_process::blocking::Pts>>,
    arg0: &str,
) -> Result<BrushShell, String> {
    let mut builtins_map =
        brush_builtins::default_builtins(brush_builtins::BuiltinSet::BashMode);

    builtins_map.insert("task".into(), builtins::builtin::<TaskCommand>());
    builtins_map.insert("setallvar".into(), builtins::builtin::<SetallvarCommand>());
    builtins_map.insert("getallvar".into(), builtins::builtin::<GetallvarCommand>());
    builtins_map.insert("listallvar".into(), builtins::builtin::<ListallvarCommand>());
    builtins_map.insert("getinput".into(), builtins::builtin::<GetinputCommand>());

    let fds = if let Some(pts) = pts {
        let pts_fd = pts.as_raw_fd();
        let mut map = HashMap::new();
        for target in 0..3 {
            let d = unsafe { libc::dup(pts_fd) };
            if d < 0 {
                return Err("dup pty slave 失败".to_string());
            }
            let file = unsafe { std::fs::File::from_raw_fd(d) };
            map.insert(target as ShellFd, OpenFile::from(file));
        }
        Some(map)
    } else {
        None
    };

    let options = CreateOptions {
        no_editing: true,
        no_profile: true,
        no_rc: true,
        interactive: true,
        login: false,
        builtins: builtins_map,
        fds,
        ..Default::default()
    };

    let mut shell = BrushShell::new(options)
        .await
        .map_err(|e| format!("创建 shell 失败: {}", e))?;

    shell.shell_name = Some(arg0.to_string());
    shell.positional_parameters = args.to_vec();
    Ok(shell)
}

// ============== 执行 task ==============
async fn execute_task(
    engine: Arc<tokio::sync::Mutex<Engine>>,
    value: &str,
    args: &[String],
) -> Result<(), i32> {
    // 1. 取出 task + 检查深度
    let (task, actioninfo) = {
        let mut e = engine.lock().await;
        if let Some(max) = e.max_depth {
            if e.task_depth >= max {
                err_line(format!(
                    "{}[错误] task 递归深度超限 ({}): {}{}",
                    color::RED, max, value, color::RESET
                ));
                return Err(1);
            }
        }
        let t = match e.tasks.get(value) {
            Some(t) => t.clone(),
            None => {
                err_line(format!(
                    "{}[错误] 找不到 task: {}{}",
                    color::RED, value, color::RESET
                ));
                return Err(127);
            }
        };
        e.task_depth += 1;
        let ai = t.settings.actioninfo.unwrap_or(e.global_actioninfo);
        (t, ai)
    };

    // 2. 打开 PTY
    let (pty, pts) = match pty_process::blocking::open() {
        Ok(p) => p,
        Err(e) => {
            err_line(format!(
                "{}[错误] 打开 pty 失败: {}{}",
                color::RED, e, color::RESET
            ));
            let mut e = engine.lock().await;
            e.task_depth -= 1;
            log_line(actioninfo, 0, "TASK", &task.name, s_fail(1));
            return Err(1);
        }
    };

    // 3. 同步 PTY 尺寸
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        let _ = pty.resize(pty_process::Size::new(rows, cols));
    }

    let pts = Arc::new(pts);

    // 4. raw mode
    let raw_guard = RawModeGuard::enter();

    // 5. SIGINT 监听
    let ctrl_c_task_name = task.name.clone();
    let ctrl_c_actioninfo = actioninfo;
    let ctrl_c_handle = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            unsafe {
                let mut term: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(libc::STDIN_FILENO, &mut term) == 0 {
                    libc::cfmakeraw(&mut term);
                    term.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
                    term.c_oflag |= libc::OPOST;
                    libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term);
                }
            }
            log_line(
                ctrl_c_actioninfo,
                0,
                "TASK",
                &ctrl_c_task_name,
                s_interrupt(),
            );
            std::process::exit(130);
        }
    });

    // 6. reader 线程：PTY master → stdout
    let raw_fd = pty.as_raw_fd();
    let dup_fd = unsafe { libc::dup(raw_fd) };
    if dup_fd < 0 {
        err_line(format!("{}[错误] dup pty fd 失败{}", color::RED, color::RESET));
        drop(raw_guard);
        ctrl_c_handle.abort();
        let mut e = engine.lock().await;
        e.task_depth -= 1;
        log_line(actioninfo, 0, "TASK", &task.name, s_fail(1));
        return Err(1);
    }
    let mut reader = unsafe { std::fs::File::from_raw_fd(dup_fd) };

    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let stdout = std::io::stdout();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut lock = stdout.lock();
                    let _ = lock.write_all(&buf[..n]);
                    let _ = lock.flush();
                }
                Err(_) => break,
            }
        }
    });

    // 7. writer 线程：stdin → PTY
    let raw_fd_w = pty.as_raw_fd();
    let dup_fd_w = unsafe { libc::dup(raw_fd_w) };
    let _writer_thread = if dup_fd_w >= 0 {
        let mut writer = unsafe { std::fs::File::from_raw_fd(dup_fd_w) };
        Some(std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if buf[..n].contains(&0x1d) {
                            break;
                        }
                        if buf[..n].contains(&0x03) {
                            std::process::exit(130);
                        }
                        if writer.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        let _ = writer.flush();
                    }
                    Err(_) => break,
                }
            }
        }))
    } else {
        None
    };

    // 8. 创建 shell
    let mut shell = match create_shell_with_pty(args, Some(pts), &task.value).await {
        Ok(s) => s,
        Err(err) => {
            err_line(format!("{}[错误] {} {}", color::RED, err, color::RESET));
            drop(pty);
            let _ = reader_thread.join();
            ctrl_c_handle.abort();
            drop(raw_guard);
            let mut e = engine.lock().await;
            e.task_depth -= 1;
            log_line(actioninfo, 0, "TASK", &task.name, s_fail(127));
            return Err(127);
        }
    };

    // 9. run_cond：在同一个 PTY shell 里先跑，非 0 则跳过
    if let Some(cond) = &task.run_cond {
        let cond = cond.trim();
        if !cond.is_empty() {
            let params = ExecutionParameters::default();
            let r = shell.run_string(cond.to_string(), &params).await;
            let passed = matches!(&r, Ok(res) if u8::from(res.exit_code) == 0);
            if !passed {
                let reason = match &r {
                    Ok(res) => format!("run_cond 退出码 {}", u8::from(res.exit_code)),
                    Err(_) => "run_cond 执行异常".to_string(),
                };
                log_line(actioninfo, 0, "TASK", &task.name, s_skip(&reason));
                drop(shell);
                drop(pty);
                let _ = reader_thread.join();
                ctrl_c_handle.abort();
                drop(raw_guard);
                {
                    let mut e = engine.lock().await;
                    e.task_depth -= 1;
                }
                if let Some(t) = _writer_thread {
                    let _ = t;
                }
                return Ok(());
            }
        }
    }

    // 10. 真正开始
    log_line(actioninfo, 0, "TASK", &task.name, s_start());

    let result = run_steps(&task.steps, &mut shell, actioninfo).await;

    // 11. 清理
    drop(shell);
    drop(pty);
    let _ = reader_thread.join();
    ctrl_c_handle.abort();
    if let Some(t) = _writer_thread {
        let _ = t;
    }
    drop(raw_guard);

    {
        let mut e = engine.lock().await;
        e.task_depth -= 1;
    }

    match &result {
        Ok(()) => log_line(actioninfo, 0, "TASK", &task.name, s_ok()),
        Err(c) => log_line(actioninfo, 0, "TASK", &task.name, s_fail(*c)),
    }
    result
}

// ============== 跑步骤 ==============
async fn run_steps(
    steps: &[Step],
    shell: &mut BrushShell,
    actioninfo: bool,
) -> Result<(), i32> {
    let mut last: Option<Result<(), i32>> = None;

    for st in steps {
        if st.require_success {
            if let Some(Err(c)) = last {
                log_line(actioninfo, 1, "STEP", &st.name, s_fail(c));
                last = Some(Err(c));
                continue;
            }
        }
        log_line(actioninfo, 1, "STEP", &st.name, s_start());

        let r = run_actions(&st.actions, shell).await;
        match &r {
            Ok(()) => log_line(actioninfo, 1, "STEP", &st.name, s_ok()),
            Err(c) => log_line(actioninfo, 1, "STEP", &st.name, s_fail(*c)),
        }
        last = Some(r);
    }

    match last {
        Some(Err(c)) => Err(c),
        _ => Ok(()),
    }
}

async fn run_actions(acts: &[Action], shell: &mut BrushShell) -> Result<(), i32> {
    for a in acts {
        run_one(a, shell).await?;
    }
    Ok(())
}

async fn run_one(a: &Action, shell: &mut BrushShell) -> Result<(), i32> {
    let script = a.run.trim();
    let params = ExecutionParameters::default();
    let result = shell.run_string(script.to_string(), &params).await;
    match result {
        Ok(exec_result) => {
            let code = u8::from(exec_result.exit_code) as i32;
            if code == 0 {
                Ok(())
            } else {
                Err(code)
            }
        }
        Err(e) => {
            err_line(format!("{}[错误] 执行失败: {}{}", color::RED, e, color::RESET));
            Err(1)
        }
    }
}

// ============== 校验 ==============
fn validate(b: &BuildFile) -> Result<(), String> {
    let mut task_vals = HashSet::new();
    for t in &b.tasks {
        if !task_vals.insert(t.value.clone()) {
            return Err(format!("task value 重复: {}", t.value));
        }
    }
    let mut input_keys = HashSet::new();
    for i in &b.inputs {
        if !input_keys.insert(i.key.clone()) {
            return Err(format!("input key 重复: {}", i.key));
        }
        match i.input_type.as_str() {
            "string" | "int" | "bool" => {}
            "only_chose" | "many_chose" => {
                if i.choose.len() < 2 {
                    return Err(format!(
                        "input `{}` (type={}) 需要至少 2 个 [[input.choose]]",
                        i.name, i.input_type
                    ));
                }
                let mut ck = HashSet::new();
                for c in &i.choose {
                    if !ck.insert(c.key.clone()) {
                        return Err(format!(
                            "input `{}` 中 choose key 重复: {}",
                            i.name, c.key
                        ));
                    }
                }
            }
            other => return Err(format!("未知 input type: {}", other)),
        }
    }
    Ok(())
}

// ============== CLI ==============
fn print_help() {
    println!(
        concat!(
            "{B}{N}{R} v{V}\n\n",
            "用法:\n",
            "  {N} <config.toml>\n",
            "  {N} --help|-h\n",
            "  {N} --version|-V\n\n",
            "说明:\n",
            "  按顺序执行配置文件中的所有 [[task]]。\n",
            "  每个 task 拥有独立、干净的 PTY shell，cd/export 只影响当前 task。\n",
            "  同一个 task 的所有命令共享同一个 PTY 终端，支持 read -p、vim、top 等。\n",
            "  交互中按 Ctrl-] 可脱离当前 PTY 会话；Ctrl+C 可中止当前 task。\n",
            "  如果配置中定义了 [[input]]，启动时会先弹 TUI 收集输入。\n",
            "    主界面回车进入填写页；Tab 在 [取消] [确认] [填写] 之间切换。\n",
            "    string/int：直接输入（int 只接受数字）；bool/only_chose：上下选；\n",
            "    many_chose：上下移动 + 空格切换选中；Enter 确认，Esc 返回主界面。\n",
            "  在 task 的脚本中取值：\n",
            "    getinput string     <key> <varname>\n",
            "    getinput int        <key> <varname>\n",
            "    getinput bool       <key> <varname>     # true / false（小写）\n",
            "    getinput only_chose <key> <varname>     # 选项的 key\n",
            "    getinput many_chose <key> <varname>     # 索引数组，如 arr=(0 2)\n",
            "  task 级别的 run_cond：在 PTY 内先执行，非 0 则跳过该 task。\n",
            "  task 级别的 require_success = true：若紧邻前一个 task 失败，则跳过该 task。\n\n",
            "配置 [settings] 项:\n",
            "  actioninfo = true|false   # 是否输出日志，默认 true\n",
            "  max_depth  = 64           # 最大递归层数，默认 64；\n",
            "                            # 设为 false 表示不限制（只允许 false）\n",
        ),
        B = color::BOLD,
        R = color::RESET,
        N = PKG_NAME,
        V = VERSION,
    );
}

fn load_build(cfg_path: &str) -> BuildFile {
    let content = match std::fs::read_to_string(cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "{}[错误] 读 {}: {}{}",
                color::RED, cfg_path, e, color::RESET
            );
            std::process::exit(1);
        }
    };
    match toml::from_str(&content) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{}[错误] 解析 toml: {}{}", color::RED, e, color::RESET);
            std::process::exit(1);
        }
    }
}

// ============== 顺序执行所有 task ==============
async fn run_all(engine: Arc<tokio::sync::Mutex<Engine>>) -> i32 {
    let task_values: Vec<String> = {
        let e = engine.lock().await;
        e.task_order.clone()
    };

    let mut last: Option<Result<(), i32>> = None;

    for value in &task_values {
        let (require_success, actioninfo, name) = {
            let e = engine.lock().await;
            let t = match e.tasks.get(value) {
                Some(t) => t,
                None => continue,
            };
            (
                t.require_success,
                t.settings.actioninfo.unwrap_or(e.global_actioninfo),
                t.name.clone(),
            )
        };

        if require_success {
            if let Some(Err(c)) = last {
                log_line(
                    actioninfo,
                    0,
                    "TASK",
                    &name,
                    s_skip(&format!("前置失败 {}", c)),
                );
                last = Some(Err(c));
                continue;
            }
        }

        let r = execute_task(engine.clone(), value, &[]).await;
        last = Some(r);
    }

    match last {
        Some(Err(c)) => c.max(1),
        _ => 0,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args
        .get(1)
        .map(|s| s == "--version" || s == "-V")
        .unwrap_or(false)
    {
        println!("{} v{}", PKG_NAME, VERSION);
        return;
    }
    if args
        .get(1)
        .map(|s| s == "--help" || s == "-h")
        .unwrap_or(false)
    {
        print_help();
        return;
    }
    if args.len() < 2 {
        print_help();
        return;
    }

    let cfg_path = &args[1];
    let build = load_build(cfg_path);

    if let Err(e) = validate(&build) {
        eprintln!("{}[错误] {}{}", color::RED, e, color::RESET);
        std::process::exit(1);
    }

    if build.tasks.is_empty() {
        eprintln!("{}[错误] 配置中没有任何 [[task]]{}", color::RED, color::RESET);
        std::process::exit(1);
    }

    // 先收集输入（同步 TUI，位于 tokio runtime 之外）
    let input_values: HashMap<String, InputValue> = if !build.inputs.is_empty() {
        match collect_inputs_sync(build.inputs.clone()) {
            Ok(v) => v,
            Err(_) => {
                eprintln!("\r\n{}[已取消输入]{}", color::RED, color::RESET);
                std::process::exit(130);
            }
        }
    } else {
        HashMap::new()
    };

    // 构建 engine
    let engine = match Engine::new(
        build.tasks,
        build.settings.actioninfo,
        build.settings.max_depth,
        build.inputs,
        input_values,
    ) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("{}[错误] {}{}", color::RED, err, color::RESET);
            std::process::exit(1);
        }
    };
    let engine_arc = Arc::new(tokio::sync::Mutex::new(engine));
    let _ = ENGINE.set(engine_arc.clone());

    // 进入 async 世界
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}[错误] 创建 runtime 失败: {}{}", color::RED, e, color::RESET);
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(run_all(engine_arc));
    std::process::exit(code);
}
