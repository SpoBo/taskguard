//! Drawing the dashboard's views.

use super::chart::{self, AXIS_W, PALETTE, Stacked, WaitStrip, spark};
use super::{App, Charts, Click, Input, NS_SORTS, RANGES, RUN_SORTS, VIEWS, View};
use crate::config::{SETTINGS, SettingKind};
use crate::dash::{self, Marker};
use crate::report::{self, chrono_like, datetime, dur, gb};
use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};

const DIM: Style = Style::new().fg(Color::DarkGray);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

pub fn draw(f: &mut Frame, app: &mut App) {
    let [head, tabs, main, foot] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(5), Constraint::Length(1)]).areas(f.area());
    draw_header(f, app, head);
    app.hits.clear();
    draw_tabs(f, app, tabs);
    app.list_rows = (0, 0, 0);
    match app.view {
        View::Overview => overview(f, app, main),
        View::Queue => queue(f, app, main),
        View::Runs => runs(f, app, main),
        View::Job => job(f, app, main),
        View::Trends => trends(f, app, main),
        View::Warnings => warnings(f, app, main),
        View::Namespaces => namespaces(f, app, main),
        View::Config => config_view(f, app, main),
        View::Help => help(f, main),
    }
    draw_footer(f, app, foot);
}

fn ns_color(app: &App, ns: &str) -> Color {
    let i = app.data.ns_list.iter().position(|n| n == ns).unwrap_or(0);
    PALETTE[i % PALETTE.len()]
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let d = &app.data;
    let mut spans = vec![
        Span::styled("taskguard top", BOLD),
        Span::raw(format!("  {}  ", chrono_like(d.now))),
        Span::raw(format!("machine {} cores, {}  ", d.ncpu, gb(d.mem_total_kb as u64))),
        Span::styled(
            if d.recorder { "recorder on  " } else { "recorder off  " },
            if d.recorder { Style::default().fg(Color::Green) } else { Style::default().fg(Color::Yellow) },
        ),
        Span::raw(format!("range {}  ", RANGES[app.range].0)),
        Span::raw(format!("ns {}  ", app.ns_filter.as_deref().unwrap_or("all"))),
    ];
    if !app.text_filter.is_empty() {
        spans.push(Span::raw(format!("filter \"{}\"  ", app.text_filter)));
    }
    if app.paused {
        spans.push(Span::styled("PAUSED  ", Style::default().fg(Color::Yellow)));
    }
    if !d.warnings.is_empty() {
        spans.push(Span::styled(
            format!("⚠ {} warnings ({})", d.warnings.len(), VIEWS.iter().position(|(v, _)| *v == View::Warnings).unwrap_or(0) + 1),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_tabs(f: &mut Frame, app: &mut App, area: Rect) {
    let mut spans = Vec::new();
    let mut x = area.x;
    // The Job view has no tab; the tab it was opened from stays marked.
    let current = if app.view == View::Job { app.back.0 } else { app.view };
    for (i, (v, name)) in VIEWS.iter().enumerate() {
        let style = if *v == current { Style::default().fg(Color::Black).bg(Color::Cyan) } else { Style::default() };
        let label = format!(" {} {} ", i + 1, name);
        let w = label.chars().count() as u16;
        app.hits.push((area.y, x, x + w - 1, Click::Tab(*v)));
        x += w + 1;
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    if app.view == View::Job {
        spans.push(Span::styled(" › job ", Style::default().fg(Color::Black).bg(Color::Cyan)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(f: &mut Frame, app: &mut App, area: Rect) {
    let text = match &app.input {
        Input::Filter(t) => Some(format!("filter keys: {t}▏   Enter apply, Esc cancel")),
        Input::ConfirmKill(pid, key) => Some(format!("send SIGTERM to {key} (pid {pid})? y / n")),
        Input::ConfirmStart(_, key) => Some(format!("start {key} now, whatever the limits say? y / n")),
        Input::EditNeeds(_, key, t) => Some(format!("needs of {key} for this run, as CORES MEMORY: {t}▏   Enter apply, Esc cancel")),
        Input::None => app.message.clone(),
    };
    if let Some(text) = text {
        f.render_widget(Paragraph::new(text).style(DIM), area);
        return;
    }
    // Each item is a key and what it does; a click on it presses the key.
    let mut items: Vec<(String, &str, Option<KeyCode>)> =
        vec![("q".into(), "quit", Some(KeyCode::Char('q'))), ("⇧←/→".into(), "tabs", None)];
    let view_items: Vec<(String, &str, Option<KeyCode>)> = match app.view {
        View::Overview => vec![
            ("t".into(), "range", Some(KeyCode::Char('t'))),
            ("n".into(), "namespace", Some(KeyCode::Char('n'))),
            ("←/→".into(), "time cursor", None),
            ("c/m/b".into(), "charts", None),
            ("o".into(), "other layer", Some(KeyCode::Char('o'))),
        ],
        View::Queue => vec![
            ("↑/↓".into(), "select", None),
            ("Enter".into(), "details", Some(KeyCode::Enter)),
            ("g".into(), "start now", Some(KeyCode::Char('g'))),
            ("e".into(), "edit needs", Some(KeyCode::Char('e'))),
            ("k".into(), "stop", Some(KeyCode::Char('k'))),
        ],
        View::Runs => vec![
            ("↑/↓".into(), "select", None),
            ("Enter".into(), "details", Some(KeyCode::Enter)),
            ("s".into(), RUN_SORTS[app.sort % RUN_SORTS.len()], Some(KeyCode::Char('s'))),
            ("r".into(), "reverse", Some(KeyCode::Char('r'))),
            ("/".into(), "filter", Some(KeyCode::Char('/'))),
        ],
        View::Namespaces => vec![
            ("s".into(), NS_SORTS[app.sort % NS_SORTS.len()], Some(KeyCode::Char('s'))),
            ("r".into(), "reverse", Some(KeyCode::Char('r'))),
            ("Enter".into(), "top job", Some(KeyCode::Enter)),
            ("t".into(), "range", Some(KeyCode::Char('t'))),
        ],
        View::Trends | View::Warnings => vec![("↑/↓".into(), "select", None), ("Enter".into(), "details", Some(KeyCode::Enter))],
        View::Job => vec![("Esc".into(), "back", Some(KeyCode::Esc))],
        View::Config => vec![
            ("↑/↓".into(), "select", None),
            ("←/-".into(), "lower", Some(KeyCode::Left)),
            ("→/+".into(), "raise", Some(KeyCode::Right)),
        ],
        View::Help => vec![],
    };
    items.extend(view_items);
    if app.view != View::Job && app.job_key.is_some() {
        items.push(("j".into(), "last job", Some(KeyCode::Char('j'))));
    }
    items.push(("space".into(), if app.paused { "resume" } else { "pause" }, Some(KeyCode::Char(' '))));
    let mut spans = Vec::new();
    let mut x = area.x;
    for (key, what, code) in items {
        let label = format!("{key} {what}");
        let w = label.chars().count() as u16;
        if let Some(code) = code {
            app.hits.push((area.y, x, x + w - 1, Click::Key(code)));
        }
        x += w + 2;
        spans.push(Span::styled(key, Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!(" {what}  "), DIM));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// -------------------------------------------------------------- overview ----

fn overview(f: &mut Frame, app: &mut App, area: Rect) {
    let [left, right] = Layout::horizontal([Constraint::Min(30), Constraint::Length(38)]).areas(area);
    app.columns = left.width.saturating_sub(AXIS_W).max(10) as usize;
    let d = &app.data;
    let n = d.machine.len();
    let charts: Vec<&str> = match app.charts {
        Charts::Both => vec!["cpu", "mem"],
        Charts::Cpu => vec!["cpu"],
        Charts::Mem => vec!["mem"],
    };
    let mut cons = vec![Constraint::Length(1)];
    cons.extend(charts.iter().map(|_| Constraint::Min(4)));
    cons.extend([Constraint::Length(1), Constraint::Length(1)]);
    let rows = Layout::vertical(cons).split(left);

    // Markers: ! a starved run ended, » a --now job started.
    let mut marks = vec![' '; n];
    for (t, m) in &d.markers {
        let i = (((t - d.from) / (d.to - d.from)) * n as f64) as usize;
        if i < n {
            marks[i] = match m {
                Marker::Starved => '!',
                Marker::Now if marks[i] != '!' => '»',
                _ => marks[i],
            };
        }
    }
    let mark_line: String = std::iter::repeat_n(' ', AXIS_W as usize).chain(marks.iter().copied()).collect();
    f.render_widget(Paragraph::new(mark_line).style(Style::default().fg(Color::Red)), rows[0]);

    for (i, which) in charts.iter().enumerate() {
        let is_cpu = *which == "cpu";
        let total: Vec<Option<f64>> = d.machine.iter().map(|b| b.has_data.then_some(if is_cpu { b.cpu } else { b.mem_kb })).collect();
        let layers: Vec<(String, Color, Vec<f64>)> =
            d.ns.iter()
                .map(|(ns, v)| (ns.clone(), ns_color(app, ns), v.iter().map(|x| if is_cpu { x.0 } else { x.1 }).collect()))
                .collect();
        // Groups in a fixed order, biggest first by what they hold now.
        let mut groups: Vec<(String, Color, Vec<f64>)> = d
            .groups
            .iter()
            .map(|(g, v)| (g.clone(), chart::group_color(g), v.iter().map(|x| if is_cpu { x.0 } else { x.1 }).collect()))
            .collect();
        groups.sort_by_key(|(g, _, _)| d.groups_now.iter().position(|x| &x.group == g).unwrap_or(usize::MAX));
        // Memory only: programs can hold more than is in use (compressed or
        // in swap). Then a line marks what is in use, in every column.
        let mark_total = !is_cpu
            && app.show_other
            && total.iter().enumerate().any(|(c, t)| {
                let sum: f64 = layers.iter().chain(groups.iter()).map(|(_, _, v)| v.get(c).copied().unwrap_or(0.0)).sum();
                t.is_some_and(|t| sum > t * 1.02)
            });
        let fmt_cpu = |v: f64| format!("{v:.0}c");
        let fmt_mem = |v: f64| if v <= 0.0 { "0G".into() } else { gb(v as u64).replace(" GB", "G").replace(" MB", "M") };
        let (max, limit, fmt, title): (f64, f64, &dyn Fn(f64) -> String, &str) = if is_cpu {
            (d.ncpu as f64, d.ncpu as f64 * app.cfg.cpu_max / 100.0, &fmt_cpu, "CPU (cores)")
        } else {
            (d.mem_total_kb, d.mem_total_kb * app.cfg.mem_max / 100.0, &fmt_mem, "Memory")
        };
        f.render_widget(
            Stacked {
                total: &total,
                layers: &layers,
                groups: &groups,
                mark_total,
                max,
                limit,
                show_other: app.show_other,
                cursor: app.cursor,
                fmt,
                title,
            },
            rows[1 + i],
        );
    }
    let strip = rows[1 + charts.len()];
    f.render_widget(WaitStrip { waiting: &d.waiting, cursor: app.cursor }, strip);
    // Time axis: start, middle, end.
    let axis = rows[2 + charts.len()];
    if axis.width > AXIS_W + 20 {
        let w = (axis.width - AXIS_W) as usize;
        let mid = chrono_like((d.from + d.to) / 2.0).to_string();
        let mut line = format!("{:<w2$}", chrono_like(d.from), w2 = w / 2 - mid.len() / 2);
        line.push_str(&mid);
        let pad = w.saturating_sub(line.chars().count() + 3);
        line.push_str(&" ".repeat(pad));
        line.push_str("now");
        f.render_widget(Paragraph::new(format!("{}{line}", " ".repeat(AXIS_W as usize))).style(DIM), axis);
    }
    legend(f, app, right);
}

fn legend(f: &mut Frame, app: &App, area: Rect) {
    let d = &app.data;
    let n = d.machine.len();
    let at = app.cursor.filter(|c| *c < n);
    let mut lines: Vec<Line> = Vec::new();
    match at {
        Some(c) => lines.push(Line::styled(format!("at {}", chrono_like(d.from + (c as f64 + 0.5) * (d.to - d.from) / n as f64)), BOLD)),
        None => lines.push(Line::styled(format!("now, and peak in the last {}", RANGES[app.range].0), BOLD)),
    }
    lines.push(Line::raw(""));
    let pick = |v: &Vec<(f64, f64)>| -> ((f64, f64), (f64, f64)) {
        let now = match at {
            Some(c) => v.get(c).copied().unwrap_or_default(),
            None => v.iter().rev().find(|x| x.0 > 0.0 || x.1 > 0.0).copied().unwrap_or_default(),
        };
        let peak = v.iter().fold((0.0f64, 0.0f64), |a, x| (a.0.max(x.0), a.1.max(x.1)));
        (now, peak)
    };
    let mut ours = (0.0, 0.0);
    for (ns, v) in &d.ns {
        let ((c, m), (pc, pm)) = pick(v);
        ours.0 += c;
        ours.1 += m;
        lines.push(Line::from(vec![
            Span::styled("██ ", Style::default().fg(ns_color(app, ns))),
            Span::styled(format!("{ns:<14}"), BOLD),
            Span::raw(format!(" {c:>4.1}c {:>8}", gb(m as u64))),
        ]));
        lines.push(Line::styled(format!("                peak {pc:>4.1}c {:>8}", gb(pm as u64)), DIM));
    }
    if d.ns.is_empty() {
        lines.push(Line::styled("no taskguard jobs in this range", DIM));
    }
    let machine_at = match at {
        Some(c) => d.machine.get(c).cloned().unwrap_or_default(),
        None => d.machine.iter().rev().find(|b| b.has_data).cloned().unwrap_or_default(),
    };
    if app.show_other {
        lines.push(Line::raw(""));
        lines.push(Line::styled("not started by taskguard:", DIM));
        let mut theirs = (0.0, 0.0);
        let mut groups: Vec<(&String, &Vec<(f64, f64)>)> = d.groups.iter().collect();
        groups.sort_by_key(|(g, _)| d.groups_now.iter().position(|x| &x.group == *g).unwrap_or(usize::MAX));
        for (g, v) in groups {
            let ((mut c, mut m), (pc, pm)) = pick(v);
            // Without a cursor, "now" is the newest reading: the newest
            // column is still filling up.
            if at.is_none() {
                let latest = d.groups_now.iter().find(|x| &x.group == g);
                (c, m) = latest.map(|x| (x.cores, x.mem_kb as f64)).unwrap_or((0.0, 0.0));
            }
            if pc.max(c) < 0.05 && pm.max(m) < 50.0 * 1024.0 {
                continue;
            }
            theirs.0 += c;
            theirs.1 += m;
            lines.push(Line::from(vec![
                Span::styled("▒▒ ", Style::default().fg(chart::group_color(g))),
                Span::styled(format!("{g:<14}"), BOLD),
                Span::raw(format!(" {c:>4.1}c {:>8}", gb(m as u64))),
            ]));
            // Now: the biggest programs in the group, which could be closed.
            match (at, d.groups_now.iter().find(|x| &x.group == g)) {
                (None, Some(x)) if !x.top.is_empty() => lines.push(Line::styled(format!("   {}", trunc(&x.top, 34)), DIM)),
                _ => lines.push(Line::styled(format!("                peak {pc:>4.1}c {:>8}", gb(pm as u64)), DIM)),
            }
        }
        lines.push(Line::from(vec![
            Span::styled("░░ ", DIM),
            Span::raw(format!(
                "{:<14} {:>4.1}c {:>8}",
                "other",
                (machine_at.cpu - ours.0 - theirs.0).max(0.0),
                gb((machine_at.mem_kb - ours.1 - theirs.1).max(0.0) as u64)
            )),
        ]));
        let beyond = ours.1 + theirs.1 - machine_at.mem_kb;
        if beyond > 512.0 * 1024.0 {
            lines.push(Line::from(vec![
                Span::styled("━━ ", Style::default().fg(Color::White)),
                Span::raw(format!("memory in use: {}", gb(machine_at.mem_kb as u64))),
            ]));
            lines.push(Line::styled(format!("   programs hold {} more,", gb(beyond as u64)), DIM));
            lines.push(Line::styled("   compressed or in swap".to_string(), DIM));
        }
    }
    lines.push(Line::from(vec![
        Span::styled("╌╌ ", Style::default().fg(Color::Yellow)),
        Span::raw(format!("limit: CPU {:.0}%, memory {:.0}%", app.cfg.cpu_max, app.cfg.mem_max)),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::styled("waiting strip, by reason:", DIM));
    lines.push(Line::from(
        [("cpu", "cpu"), ("memory", "mem"), ("slots", "slots"), ("reserved", "resv"), ("learning", "learn")]
            .iter()
            .flat_map(|(b, short)| [Span::styled("█", Style::default().fg(chart::blocker_color(b))), Span::raw(format!("{short} "))])
            .collect::<Vec<_>>(),
    ));
    lines.push(Line::from(vec![
        Span::styled("! ", Style::default().fg(Color::Red)),
        Span::raw("starved run  "),
        Span::styled("» ", Style::default().fg(Color::Red)),
        Span::raw("--now start"),
    ]));
    if at.is_some() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("jobs running then:", BOLD));
        for (ns, k) in d.cursor_runs.iter().take(10) {
            lines.push(Line::from(vec![Span::styled("■ ", Style::default().fg(ns_color(app, ns))), Span::raw(trunc(k, 33))]));
        }
        if d.cursor_runs.is_empty() {
            lines.push(Line::styled("(none)", DIM));
        }
    }
    f.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::LEFT).border_style(DIM)), area);
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

fn table_state(app: &App) -> TableState {
    TableState::default().with_selected(Some(app.sel))
}

fn remember_rows(app: &mut App, area: Rect, header: u16, state: &TableState, len: usize) {
    let visible = area.height.saturating_sub(header) as usize;
    let first = state.offset();
    app.list_rows = (area.y + header, visible.min(len.saturating_sub(first)), first);
}

/// Draw a table. When it has more rows than fit, the last line says how many
/// rows are hidden above and below.
fn render_list(f: &mut Frame, app: &mut App, table: Table, area: Rect, header: u16, len: usize) {
    let fits = (area.height.saturating_sub(header) as usize) >= len;
    let (list, hint) = if fits || area.height <= header + 2 {
        (area, None)
    } else {
        let [list, hint] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        (list, Some(hint))
    };
    let mut st = table_state(app);
    f.render_stateful_widget(table, list, &mut st);
    remember_rows(app, list, header, &st, len);
    if let Some(hint) = hint {
        let shown = app.list_rows.1;
        let above = st.offset();
        let below = len.saturating_sub(above + shown);
        let mut parts = Vec::new();
        if above > 0 {
            parts.push(format!("↑ {above} more above"));
        }
        if below > 0 {
            parts.push(format!("↓ {below} more below"));
        }
        let text = format!("  {}   (↑/↓ or the mouse wheel to scroll)", parts.join("   "));
        f.render_widget(Paragraph::new(text).style(Style::default().fg(Color::Cyan)), hint);
    }
}

/// A bar in colour: in use within the estimates, in use above them (red),
/// promised to running jobs (yellow), free, and the limit.
fn color_bar(used: f64, over: f64, reserved: f64, total: f64, limit: f64, width: usize) -> Vec<Span<'static>> {
    if total <= 0.0 {
        return vec![Span::raw("·".repeat(width))];
    }
    let cell = |v: f64| ((v / total) * width as f64).round().clamp(0.0, width as f64) as usize;
    let u = cell(used);
    let o = u - cell(used - over.min(used)).min(u);
    let r = cell(used + reserved).max(u);
    let l = (((limit / total) * width as f64).floor().max(0.0) as usize).min(width.saturating_sub(1));
    let mut spans: Vec<Span> = Vec::new();
    for i in 0..width {
        let (ch, color) = if i == l && l + 1 < width {
            ("│", Color::Yellow)
        } else if i < u - o {
            ("█", Color::Gray)
        } else if i < u {
            ("█", Color::Red)
        } else if i < r {
            ("▒", Color::Yellow)
        } else {
            ("·", Color::DarkGray)
        };
        spans.push(Span::styled(ch, Style::default().fg(color)));
    }
    spans
}

// ----------------------------------------------------------------- queue ----

fn queue(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(s) = app.data.snap.as_ref() else {
        f.render_widget(Paragraph::new("no queue data"), area);
        return;
    };
    let m = &s.machine;
    let (over_cpu, over_mem) = crate::queue::over(&s.running);
    let over_text = |v: String, over: bool| if over { format!("  {v} over estimates") } else { String::new() };
    let mut cpu = vec![Span::raw("CPU     [")];
    cpu.extend(color_bar(m.cpu_busy, over_cpu, s.reserve_cpu, m.ncpu as f64, s.cpu_limit, 24));
    cpu.push(Span::raw(format!(
        "]  {:>5.1} busy  +{:.1} promised  of {:.1} cores  limit {:.0}%",
        m.cpu_busy, s.reserve_cpu, m.ncpu as f64, s.limits.cpu_max_pct
    )));
    cpu.push(Span::styled(over_text(format!("{over_cpu:.1} cores"), over_cpu >= 0.05), Style::default().fg(Color::Red)));
    let mut mem = vec![Span::raw("MEMORY  [")];
    mem.extend(color_bar(m.mem_used_kb as f64, over_mem as f64, s.reserve_mem_kb as f64, m.mem_total_kb as f64, s.mem_limit_kb as f64, 24));
    mem.push(Span::raw(format!(
        "]  {} held  +{} promised  of {}  limit {:.0}%",
        gb(m.mem_used_kb),
        gb(s.reserve_mem_kb),
        gb(m.mem_total_kb),
        s.limits.mem_max_pct
    )));
    mem.push(Span::styled(over_text(gb(over_mem), over_mem >= 1024), Style::default().fg(Color::Red)));
    let bars = vec![
        Line::from(cpu),
        Line::from(mem),
        Line::from(vec![
            Span::raw("         "),
            Span::styled("█", Style::default().fg(Color::Gray)),
            Span::styled(" in use   ", DIM),
            Span::styled("█", Style::default().fg(Color::Red)),
            Span::styled(" jobs above their estimate   ", DIM),
            Span::styled("▒", Style::default().fg(Color::Yellow)),
            Span::styled(" promised to running jobs   ", DIM),
            Span::styled("│", Style::default().fg(Color::Yellow)),
            Span::styled(" limit", DIM),
        ]),
    ];
    let n_rows = s.waiting.len() + s.running.len();
    let [top, list, why] =
        Layout::vertical([Constraint::Length(4), Constraint::Length((n_rows as u16 + 2).clamp(3, 14)), Constraint::Min(6)]).areas(area);
    f.render_widget(Paragraph::new(bars), top);

    let est = |e: &crate::queue::Entry| {
        if e.by_hand.is_some() {
            " set"
        } else if e.known || e.raised_by_min {
            ""
        } else {
            " est"
        }
    };
    let mut rows: Vec<Row> = Vec::new();
    for e in &s.running {
        let over =
            e.live_cpu > e.start_need_cpu.unwrap_or(e.need_cpu) + 0.05 || e.live_mem_kb > e.start_need_mem_kb.unwrap_or(e.need_mem_kb);
        rows.push(Row::new(vec![
            Cell::from(if e.now { "NOW" } else { "RUN" }).style(Style::default().fg(Color::Green)),
            Cell::from(trunc(&e.key, 44)),
            Cell::from(e.ns.clone()),
            Cell::from(dur(s.now - e.started_at.unwrap_or(s.now))),
            Cell::from(format!("{:.1}c {} now", e.live_cpu, gb(e.live_mem_kb))).style(if over {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            }),
            Cell::from(format!("needs {:.1}c {}{}", e.need_cpu, gb(e.need_mem_kb), est(e))),
        ]));
    }
    for w in &s.waiting {
        let e = &w.entry;
        let blocked = report::main_blocker(&w.decision).map(|b| b.name().to_uppercase()).unwrap_or_else(|| "starting".into());
        rows.push(Row::new(vec![
            Cell::from("WAIT").style(Style::default().fg(Color::Yellow)),
            Cell::from(trunc(&e.key, 44)),
            Cell::from(e.ns.clone()),
            Cell::from(dur(s.now - e.queued_at)),
            Cell::from(format!("{:.1}c {}{}", e.need_cpu, gb(e.need_mem_kb), est(e))),
            Cell::from(blocked)
                .style(Style::default().fg(chart::blocker_color(report::main_blocker(&w.decision).map(|b| b.name()).unwrap_or("")))),
        ]));
    }
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Min(30),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(18),
            Constraint::Min(14),
        ],
    )
    .header(Row::new(vec!["", "job", "namespace", "time", "needs / now", "blocked by"]).style(BOLD))
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .block(Block::new().borders(Borders::TOP).title(format!(" {} running, {} waiting ", s.running.len(), s.waiting.len())));
    render_list(f, app, table, list, 2, n_rows);

    // The Why panel.
    let Some(s) = app.data.snap.as_ref() else { return };
    let mut lines: Vec<Line> = Vec::new();
    if let Some(w) = app.queue_waiting() {
        let e = &w.entry;
        lines.push(Line::styled(format!("Why is {} waiting?", e.key), BOLD));
        for (ok, text) in report::rule_checks(s, w) {
            lines.push(Line::from(vec![
                Span::styled(if ok { " ✓ " } else { " ✗ " }, Style::default().fg(if ok { Color::Green } else { Color::Red })),
                Span::raw(text),
            ]));
        }
        if let Some(next) = &w.next {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![Span::styled("What unblocks it: ", BOLD), Span::raw(next.clone())]));
        }
        if let Some(b) = report::main_blocker(&w.decision)
            && b.name() == "memory"
        {
            let groups: Vec<String> = app
                .data
                .groups_now
                .iter()
                .filter(|g| g.mem_kb >= 256 * 1024)
                .take(3)
                .map(|g| format!("{} {} ({})", g.group, gb(g.mem_kb), g.top))
                .collect();
            if !groups.is_empty() {
                lines.push(Line::raw(format!("  outside taskguard now: {}", groups.join("; "))));
            } else if let Some((name, mem, _)) = s.top_other.first() {
                lines.push(Line::raw(format!("  the biggest user outside taskguard is {name} ({})", gb(*mem))));
            }
        }
        let since = e.blocker_since.map(|t| format!(" for {}", dur(s.now - t))).unwrap_or_default();
        lines.push(Line::raw(format!(
            "Waiting {} in total; the current reason has held{since}. {}",
            dur(s.now - e.queued_at),
            if e.bypassed_since.is_some() { "Newer jobs have passed it; after 2 minutes it gets a reservation." } else { "" }
        )));
        if let Some(h) = &e.by_hand {
            lines.push(Line::styled(format!("Needs {h}, for this run only."), Style::default().fg(Color::Cyan)));
        } else if !e.known && !e.raised_by_min {
            lines.push(Line::styled(
                format!(
                    "First run: nothing is learned yet, so it reserves an estimate ({}). The run teaches its real needs.",
                    e.estimate_from.as_deref().unwrap_or("a default")
                ),
                DIM,
            ));
        }
        lines.push(Line::styled("g start it now   e change its needs for this run   Enter details", DIM));
    } else if let Some((e, _)) = app.queue_row(app.sel) {
        lines.push(Line::styled(format!("{} is running", e.key), BOLD));
        lines.push(Line::raw(format!(
            "now {:.1} cores, {}; needs {:.1} cores, {} ({}); started {} ago",
            e.live_cpu,
            gb(e.live_mem_kb),
            e.need_cpu,
            gb(e.need_mem_kb),
            if e.by_hand.is_some() {
                "set by hand"
            } else if e.known {
                "learned"
            } else if e.raised_by_min {
                "minimum"
            } else {
                "estimate"
            },
            dur(s.now - e.started_at.unwrap_or(s.now))
        )));
        if e.live_cpu > e.start_need_cpu.unwrap_or(e.need_cpu) + 0.05 || e.live_mem_kb > e.start_need_mem_kb.unwrap_or(e.need_mem_kb) {
            lines.push(Line::styled(
                "it uses more than its estimate; its needs now follow its peak, and the next run learns them",
                Style::default().fg(Color::Red),
            ));
        }
        if let Some(d) = e.est_dur_s {
            lines.push(Line::raw(format!("usually takes {}, so about {} left", dur(d), dur(d - (s.now - e.started_at.unwrap_or(s.now))))));
        }
        if e.now {
            lines.push(Line::raw("it skipped the queue (--now); it is measured like any other job"));
        }
        lines.push(Line::styled("Enter details: CPU and memory over time   e lower what it still reserves", DIM));
    } else {
        lines.push(Line::styled("The queue is empty. Every job started as soon as it arrived.", DIM));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::new().borders(Borders::TOP)), why);
}

// ------------------------------------------------------------------ runs ----

fn runs(f: &mut Frame, app: &mut App, area: Rect) {
    let [list, detail] = Layout::vertical([Constraint::Min(5), Constraint::Length(7)]).areas(area);
    let d = &app.data;
    let rows: Vec<Row> = d
        .runs
        .iter()
        .map(|r| {
            let mut flags = Vec::new();
            if r.starved.is_some() {
                flags.push("STARVED");
            }
            if r.now {
                flags.push("NOW");
            }
            let style = if r.starved.is_some() { Style::default().fg(Color::Red) } else { Style::default() };
            Row::new(vec![
                Cell::from(datetime(r.ended_at)),
                Cell::from(r.ns.clone()),
                Cell::from(r.label.clone().unwrap_or_default()),
                Cell::from(trunc(&r.key, 44)),
                Cell::from(dur(r.waited_s)),
                Cell::from(dur(r.dur_s)),
                Cell::from(format!("{} / {}", fmt_opt(r.cores_used), fmt_opt(r.cores_wanted))),
                Cell::from(r.peak_mem_kb.map(gb).unwrap_or("-".into())),
                Cell::from(r.exit.map(|e| e.to_string()).unwrap_or("-".into())),
                Cell::from(r.main_blocker.clone().unwrap_or_default()),
                Cell::from(flags.join(" ")),
            ])
            .style(style)
        })
        .collect();
    let n = rows.len();
    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(12),
            Constraint::Length(9),
            Constraint::Min(24),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Length(4),
            Constraint::Length(9),
            Constraint::Length(11),
        ],
    )
    .header(
        Row::new(vec!["ended", "namespace", "kind", "job", "waited", "took", "cores u/w", "peak", "exit", "blocker", "flags"]).style(BOLD),
    )
    .row_highlight_style(Style::default().bg(Color::DarkGray));
    render_list(f, app, table, list, 1, n);

    let d = &app.data;
    let mut lines = Vec::new();
    if let Some(r) = d.runs.get(app.sel) {
        lines.push(Line::styled(format!("{}  ({})", r.key, datetime(r.ended_at)), BOLD));
        if d.run_spans.is_empty() {
            lines.push(Line::raw(if r.waited_s < 1.0 { "started at once".to_string() } else { format!("waited {}", dur(r.waited_s)) }));
        } else {
            lines.push(Line::raw(format!("why it waited: {}", dash::blocker_history(&d.run_spans))));
            if let Some((_, _, detail)) = d.run_spans.last() {
                lines.push(Line::styled(format!("  last reason: {detail}"), DIM));
            }
        }
        for e in &r.starved_detail {
            lines.push(Line::styled(format!("possibly starved: {e}"), Style::default().fg(Color::Red)));
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::new().borders(Borders::TOP)), detail);
}

fn fmt_opt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.1}")).unwrap_or("-".into())
}

// ------------------------------------------------------------------- job ----

fn job(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(j) = app.data.job.as_ref() else {
        f.render_widget(Paragraph::new("pick a job: select a row in Runs, Trends or Warnings and press Enter"), area);
        return;
    };
    let l = &j.learned;
    let mut lines = vec![Line::styled(j.key.clone(), BOLD)];
    let cpu_src = match (l.cpu, j.min_cpu) {
        (Some(c), Some(m)) if m > c => format!("{m:.1} cores (minimum; learned {c:.1})"),
        (Some(c), _) => format!("{c:.1} cores (learned, median of the last {} runs, from cores wanted)", l.runs),
        (None, Some(m)) => format!("{m:.1} cores (minimum)"),
        (None, None) => "unknown yet".into(),
    };
    let mem_src = match (l.mem_kb, j.min_mem_kb) {
        (Some(v), Some(m)) if m > v => format!("{} (minimum; learned {})", gb(m), gb(v)),
        (Some(v), _) if l.mem_boosted => {
            format!("{} (learned peak {} + 25% after a memory-starved run)", gb((v as f64 * 1.25) as u64), gb(v))
        }
        (Some(v), _) => format!("{} (learned, highest peak of the last {} runs)", gb(v), l.runs),
        (None, Some(m)) => format!("{} (minimum)", gb(m)),
        (None, None) => "unknown yet".into(),
    };
    lines.push(Line::raw(format!("needs CPU:    {cpu_src}")));
    lines.push(Line::raw(format!("needs memory: {mem_src}")));
    lines.push(Line::raw(format!("usually takes {}", l.dur_s.map(dur).unwrap_or("?".into()))));
    lines.push(Line::raw(""));
    if let Some(live) = &j.live {
        live_run(&mut lines, live, app.data.now, area.width.saturating_sub(24) as usize);
        lines.push(Line::raw(""));
    }
    let chrono: Vec<&dash::RunRow> = j.runs.iter().rev().collect();
    let series = |f: &dyn Fn(&dash::RunRow) -> Option<f64>| -> Vec<f64> { chrono.iter().filter_map(|r| f(r)).collect() };
    let w = 40;
    lines.push(Line::raw(format!("duration      {}", spark(&series(&|r| Some(r.dur_s)), w))));
    lines.push(Line::raw(format!("cores used    {}", spark(&series(&|r| r.cores_used), w))));
    lines.push(Line::raw(format!("cores wanted  {}", spark(&series(&|r| r.cores_wanted), w))));
    lines.push(Line::raw(format!("peak memory   {}", spark(&series(&|r| r.peak_mem_kb.map(|v| v as f64)), w))));
    lines.push(Line::styled("              oldest → newest", DIM));
    if !j.adjustments.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("what taskguard changed on its own:", BOLD));
        for a in j.adjustments.iter().take(4) {
            lines.push(Line::raw(format!("  {}  {} - {}", datetime(a.ts), dash::adjustment_text(&a.kind, a.old, a.new), a.reason)));
        }
    }
    let head_h = lines.len() as u16 + 1;
    let [top, list] = Layout::vertical([Constraint::Length(head_h), Constraint::Min(3)]).areas(area);
    f.render_widget(Paragraph::new(lines), top);
    let rows: Vec<Row> = j
        .runs
        .iter()
        .map(|r| {
            Row::new(vec![
                datetime(r.ended_at),
                dur(r.waited_s),
                dur(r.dur_s),
                format!("{} / {}", fmt_opt(r.cores_used), fmt_opt(r.cores_wanted)),
                r.peak_mem_kb.map(gb).unwrap_or("-".into()),
                r.exit.map(|e| e.to_string()).unwrap_or("-".into()),
                r.main_blocker.clone().unwrap_or_default(),
                r.starved.clone().map(|s| format!("STARVED ({s})")).unwrap_or_default(),
            ])
            .style(if r.starved.is_some() { Style::default().fg(Color::Red) } else { Style::default() })
        })
        .collect();
    let n = rows.len();
    let t = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Length(4),
            Constraint::Length(9),
            Constraint::Min(10),
        ],
    )
    .header(Row::new(vec!["ended", "waited", "took", "cores u/w", "peak", "exit", "blocker", ""]).style(BOLD))
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .block(Block::new().borders(Borders::TOP).title(" last runs "));
    render_list(f, app, t, list, 2, n);
}

/// The run that is going on now: its load over time, next to its needs, and
/// the signs of starvation that the end of the run will judge.
fn live_run(lines: &mut Vec<Line<'static>>, live: &super::LiveRun, now: f64, w: usize) {
    let e = &live.entry;
    let red = Style::default().fg(Color::Red);
    if live.waiting {
        lines.push(Line::styled(format!("WAITING for {}  {}", dur(now - e.queued_at), e.blocker.clone().unwrap_or_default()), BOLD));
        return;
    }
    let started = e.started_at.unwrap_or(now);
    lines.push(Line::styled(format!("RUNNING for {}  (this run, oldest → now)", dur(now - started)), BOLD));
    let s = &live.samples;
    let w = w.clamp(10, 80);
    let used: Vec<f64> = s.iter().map(|x| x.1).collect();
    let wanted: Vec<f64> = s.iter().map(|x| x.2).collect();
    let mem: Vec<f64> = s.iter().map(|x| x.3 as f64).collect();
    let peak_mem = s.iter().map(|x| x.3).max().unwrap_or(0);
    let peak_cpu = used.iter().copied().fold(0.0, f64::max);
    let over = |v: bool| if v { red } else { Style::default() };
    lines.push(Line::from(vec![
        Span::raw(format!("cores used    {}  ", spark(&used, w))),
        Span::styled(format!("now {:.1}, peak {peak_cpu:.1}, needs {:.1}", e.live_cpu, e.need_cpu), over(e.live_cpu > e.need_cpu + 0.05)),
    ]));
    lines.push(Line::raw(format!("cores wanted  {}", spark(&wanted, w))));
    lines.push(Line::from(vec![
        Span::raw(format!("memory        {}  ", spark(&mem, w))),
        Span::styled(
            format!("now {}, peak {}, needs {}", gb(e.live_mem_kb), gb(peak_mem), gb(e.need_mem_kb)),
            over(peak_mem > e.need_mem_kb),
        ),
    ]));
    // The last 30 seconds, with the thresholds the end of the run uses.
    let recent: Vec<_> = s.iter().filter(|x| x.0 >= now - 30.0).collect();
    if recent.is_empty() {
        lines.push(Line::styled("no samples yet", DIM));
        return;
    }
    let n = recent.len() as f64;
    let (u, wa, pg) = recent.iter().fold((0.0, 0.0, 0.0), |a, x| (a.0 + x.1 / n, a.1 + x.2 / n, a.2 + x.4 / n));
    let wait_ratio = if u > 0.05 { (wa - u).max(0.0) / u } else { 0.0 };
    let cpu_starved = wait_ratio > crate::insight::CPU_WAIT_RATIO;
    lines.push(Line::from(vec![
        Span::styled(if cpu_starved { " ✗ " } else { " ✓ " }, if cpu_starved { red } else { Style::default().fg(Color::Green) }),
        Span::raw(format!(
            "CPU: its threads wait for a core {:.0}% as long as they run (starved above {:.0}%); wants {wa:.1} cores, gets {u:.1}",
            wait_ratio * 100.0,
            crate::insight::CPU_WAIT_RATIO * 100.0
        )),
    ]));
    let mem_starved = pg > crate::insight::PAGEINS_PER_S;
    lines.push(Line::from(vec![
        Span::styled(if mem_starved { " ✗ " } else { " ✓ " }, if mem_starved { red } else { Style::default().fg(Color::Green) }),
        Span::raw(format!(
            "memory: {pg:.0} page-ins per second (short of memory above {:.0}, while the machine is under memory pressure)",
            crate::insight::PAGEINS_PER_S
        )),
    ]));
    if let Some(h) = &e.by_hand {
        lines.push(Line::styled(format!("needs {h}, for this run only"), Style::default().fg(Color::Cyan)));
    }
}

// ---------------------------------------------------------------- config ----

fn config_view(f: &mut Frame, app: &mut App, area: Rect) {
    let user = crate::config::user_config_path().display().to_string();
    let rows: Vec<Row> = SETTINGS
        .iter()
        .map(|set| {
            let origin = app.cfg.origin.get(set.key).cloned().unwrap_or_else(|| "built-in".into());
            let elsewhere = origin != "built-in" && origin != user;
            let kind = match set.kind {
                SettingKind::Bool => "on / off",
                SettingKind::Number(..) => "number",
                SettingKind::Size(..) => "size",
            };
            Row::new(vec![
                Cell::from(set.key),
                Cell::from(app.cfg.setting_text(set.key)).style(BOLD),
                Cell::from(if elsewhere { format!("{origin} (wins over your change)") } else { origin }).style(if elsewhere {
                    Style::default().fg(Color::Yellow)
                } else {
                    DIM
                }),
                Cell::from(format!("{} ({kind})", set.what)),
            ])
        })
        .collect();
    let [top, list] = Layout::vertical([Constraint::Length(3), Constraint::Min(4)]).areas(area);
    f.render_widget(
        Paragraph::new(vec![
            Line::raw(format!("Changes are saved in {user}. They apply to every job on this machine,")),
            Line::raw("waiting jobs included, unless a repo's .taskguard.toml or a [dir] section sets the same thing."),
            Line::styled("The limits are the headroom: jobs start only while the machine stays under them.", DIM),
        ]),
        top,
    );
    let n = rows.len();
    let table = Table::new(rows, [Constraint::Length(17), Constraint::Length(8), Constraint::Length(34), Constraint::Min(20)])
        .header(Row::new(vec!["setting", "value", "set by", "what it does"]).style(BOLD))
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .block(Block::new().borders(Borders::TOP));
    render_list(f, app, table, list, 2, n);
}

// ---------------------------------------------------------------- trends ----

fn pct_cell(t: Option<(f64, f64, f64)>) -> Cell<'static> {
    match t {
        Some((p, _, _)) => {
            let color = if p.abs() > 50.0 {
                Color::Red
            } else if p.abs() > 25.0 {
                Color::Yellow
            } else {
                Color::Reset
            };
            Cell::from(format!("{p:+.0}%")).style(Style::default().fg(color))
        }
        None => Cell::from("-"),
    }
}

fn trends(f: &mut Frame, app: &mut App, area: Rect) {
    let [list, detail] = Layout::vertical([Constraint::Min(5), Constraint::Length(3)]).areas(area);
    let d = &app.data;
    let rows: Vec<Row> = d
        .trends
        .iter()
        .map(|t| {
            Row::new(vec![
                Cell::from(trunc(&t.key, 44)),
                Cell::from(t.ns.clone()),
                Cell::from(t.runs.to_string()),
                pct_cell(t.mem),
                pct_cell(t.cpu),
                pct_cell(t.dur),
                Cell::from(spark(&t.spark_mem, 24)),
            ])
        })
        .collect();
    let n = rows.len();
    let table = Table::new(
        rows,
        [
            Constraint::Min(24),
            Constraint::Length(12),
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(26),
        ],
    )
    .header(Row::new(vec!["job", "namespace", "runs", "memory", "cores", "duration", "memory over all runs"]).style(BOLD))
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .block(Block::new().title(" median of the last 10 runs against the 10 before them; yellow > 25%, red > 50% "));
    render_list(f, app, table, list, 2, n);
    let text = match app.data.trends.get(app.sel) {
        Some(t) => dash::trend_text(t),
        None => "no trends yet: a job needs 13 runs before its last 10 can be compared".into(),
    };
    f.render_widget(Paragraph::new(text).block(Block::new().borders(Borders::TOP)), detail);
}

// -------------------------------------------------------------- warnings ----

fn warnings(f: &mut Frame, app: &mut App, area: Rect) {
    let d = &app.data;
    let mut lines: Vec<Line> = Vec::new();
    if d.warnings.is_empty() {
        lines.push(Line::styled("No warnings in the last 7 days: no starved runs, no big trends.", DIM));
    }
    for (i, w) in d.warnings.iter().enumerate() {
        let sel = i == app.sel;
        let color = match w.kind {
            "starved" => Color::Red,
            "repeated" => Color::LightRed,
            "trend" => Color::Yellow,
            _ => Color::Magenta,
        };
        let mut title = Style::default().fg(color).add_modifier(Modifier::BOLD);
        if sel {
            title = title.bg(Color::DarkGray);
        }
        lines.push(Line::from(vec![Span::styled(format!("{} ", datetime(w.ts)), DIM), Span::styled(w.title.clone(), title)]));
        for l in &w.lines {
            lines.push(Line::raw(format!("     {l}")));
        }
    }
    // Keep the selected warning in view.
    let before: usize = d.warnings.iter().take(app.sel).map(|w| 1 + w.lines.len()).sum();
    let scroll = before.saturating_sub(area.height as usize / 3) as u16;
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((scroll, 0)), area);
}

// ------------------------------------------------------------ namespaces ----

fn namespaces(f: &mut Frame, app: &mut App, area: Rect) {
    let rows: Vec<Row> = app
        .data
        .namespaces
        .iter()
        .map(|n| {
            Row::new(vec![
                Cell::from(format!("██ {}", n.ns)).style(Style::default().fg(ns_color(app, &n.ns))),
                Cell::from(format!("{:.2}", n.cpu_hours)),
                Cell::from(format!("{:.2}", n.gb_hours)),
                Cell::from(n.runs.to_string()),
                Cell::from(dur(n.wait_s)),
                Cell::from(n.failed.to_string()),
                Cell::from(n.starved.to_string()).style(if n.starved > 0 { Style::default().fg(Color::Red) } else { Style::default() }),
                Cell::from(n.top_keys.iter().map(|(k, h)| format!("{} ({h:.2}h)", trunc(k, 30))).collect::<Vec<_>>().join(", ")),
            ])
        })
        .collect();
    let n = rows.len();
    let table = Table::new(
        rows,
        [
            Constraint::Length(18),
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Min(20),
        ],
    )
    .header(Row::new(vec!["namespace", "CPU-hours", "GB-hours", "runs", "waited", "failed", "starved", "top jobs by CPU"]).style(BOLD))
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .block(Block::new().title(format!(" last {} ", RANGES[app.range].0)));
    render_list(f, app, table, area, 2, n);
}

// ------------------------------------------------------------------ help ----

fn help(f: &mut Frame, area: Rect) {
    let text = "\
VIEWS
  1 Overview    machine load over time, with taskguard's jobs stacked per namespace
  2 Queue       what runs and what waits, and a Why panel for the selected job
  3 Runs        past runs; the panel below shows why the selected run waited
  4 Trends      commands whose memory, CPU or duration grows
  5 Warnings    possibly starved runs, suggested minimums, trend alerts, --now runs over a limit
  6 Namespaces  load and waits per namespace
  7 Config      limits and other settings, changed with ←/→ and saved in your config
  8 Help        this page
  Enter on a row opens the job: its learned needs, its history, and the run going on now.
  Esc or Backspace goes back; j opens the last job again.

KEYS
  q quit   1-8 / Tab / Shift, Option or Cmd + ←/→ views   t / T time range   n next namespace
  / filter jobs   Esc clear or back   ←/→ time cursor (Overview)   c / m / b charts   o the \"other\" layer
  ↑/↓ select   Enter details   s sort   r reverse   space pause
  Queue: g start a waiting job now   e change a job's needs for this run   k stop a job
  mouse: click a tab, a key in the bottom line, or a row; the wheel scrolls lists and zooms the Overview

CHART LAYERS (Overview)
  ██ coloured   load of jobs taskguard started, one colour per namespace
  ▒▒ muted      load taskguard did not start, per group: agents (with what they started), browsers,
                editors, dev services, containers, chat & apps; the legend names the biggest programs
  ░░ grey       the rest of the machine's load
  ━━ white      memory in use, where the layers pass it: a program's memory also counts what
                the system compressed or moved to swap, so programs can add up to more
  ╌╌ yellow     the limit (cpu_max, mem_max)
  waiting row   how many jobs waited, coloured by the main reason:
                red CPU, magenta memory, yellow slots, blue reservation, cyan learning
  ! / »         a run that was possibly starved ended / a --now job started

HOW A JOB IS ADMITTED
  CPU busy + CPU promised to running jobs + this job's CPU need   must fit under cpu_max
  memory held + memory promised + this job's memory need          must fit under mem_max
  needs are learned from past runs (memory: highest peak; CPU: median of cores wanted)
  a first run just needs live room; nothing running means it always starts
";
    f.render_widget(Paragraph::new(text), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::{Db, NewRun, RunResult, now};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect()
    }

    /// A fixed database: machine load for ten minutes, one namespace's job in
    /// the second half, one starved run, and a waiting span.
    fn fixture() -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        let t0 = now() - 600.0;
        for i in 0..300 {
            let s = crate::machine::MachineSample {
                ts: t0 + i as f64 * 2.0,
                cpu_busy: 6.0,
                cpu_inst: 6.0,
                ncpu: 10,
                mem_used_kb: 16 << 20,
                mem_total_kb: 32 << 20,
                ..Default::default()
            };
            db.machine_sample(&s, 0, 1).unwrap();
        }
        let id = db.insert_run(&NewRun { ns: "dalp", key: "packages/api:vitest_run", ..Default::default() }).unwrap();
        db.mark_started(id, t0 + 300.0, 12.0, Some("memory")).unwrap();
        for i in 0..150 {
            db.job_sample(id, t0 + 300.0 + i as f64 * 2.0, 2.0, 4.0, 6.0, 4 << 20, 0.0).unwrap();
        }
        db.wait_span(id, t0 + 288.0, t0 + 300.0, "memory", "memory: would reach 91%").unwrap();
        db.finish_run(
            id,
            &RunResult {
                ended_at: t0 + 590.0,
                exit: 0,
                peak_mem_kb: 4 << 20,
                cores_used: 4.0,
                cores_wanted: 6.0,
                measured: true,
                starved: Some("cpu".into()),
                starved_detail: Some("[\"waited on CPU 33% of its run time (wanted 6.0 cores, got 4.0)\"]".into()),
                ..Default::default()
            },
        )
        .unwrap();
        db.adjustment("packages/api:vitest_run", id, "cpu_need", Some(3.0), Some(6.0), "waited on CPU").unwrap();
        let mut app = App::new(tmp.path().to_path_buf(), Config::default());
        app.range = 1; // 15 minutes
        app.columns = 100;
        app.load().unwrap();
        (tmp, app)
    }

    #[test]
    fn overview_draws_layers_legend_and_markers() {
        let (_t, mut app) = fixture();
        let s = screen(&mut app, 150, 40);
        assert!(s.contains("CPU (cores)"), "{s}");
        assert!(s.contains("Memory"));
        assert!(s.contains("dalp"), "the namespace is in the legend:\n{s}");
        assert!(s.contains("other"));
        assert!(s.contains("█") && s.contains("░"), "both our layer and the other layer are drawn:\n{s}");
        assert!(s.contains('!'), "the starved run is marked:\n{s}");
        assert!(s.contains("⚠"), "the warning badge shows on every view");
    }

    #[test]
    fn programs_outside_taskguard_are_shown_by_group() {
        let (_t, mut app) = fixture();
        let db = crate::db::Db::open_dir(&app.dir).unwrap();
        for i in 0..30 {
            let t = now() - 60.0 + i as f64 * 2.0;
            let g = |group: &str, cores, mem_kb, top: &str| crate::db::GroupReading {
                group: group.into(),
                cores,
                mem_kb,
                procs: 1,
                top: top.into(),
            };
            db.group_samples(
                t,
                &[g("agents", 1.5, 6 << 20, "claude ×20 5.0 GB, bun ×8 1.0 GB"), g("browsers", 0.2, 1 << 20, "Google Chrome ×12 1.0 GB")],
            )
            .unwrap();
        }
        app.load().unwrap();
        let s = screen(&mut app, 160, 40);
        assert!(s.contains("not started by taskguard"), "{s}");
        let (agents, browsers) = (s.find("agents").unwrap(), s.find("browsers").unwrap());
        assert!(agents < browsers, "the biggest group comes first:\n{s}");
        assert!(s.contains("6.0 GB") && s.contains("claude ×20"), "{s}");
        assert!(s.contains('▒'), "the groups are drawn in the chart:\n{s}");
        app.show_other = false;
        let s = screen(&mut app, 160, 40);
        assert!(!s.contains("not started by taskguard"), "o hides everything taskguard did not start");
    }

    #[test]
    fn groups_are_drawn_in_full_above_the_machine_total() {
        // The machine holds 16 GB (fixture); the job 4 GB and the agents 20 GB,
        // with memory compressed or in swap.
        let (_t, mut app) = fixture();
        let db = crate::db::Db::open_dir(&app.dir).unwrap();
        for i in 0..300 {
            let t = now() - 600.0 + i as f64 * 2.0;
            let agents =
                crate::db::GroupReading { group: "agents".into(), cores: 1.0, mem_kb: 20 << 20, procs: 9, top: "claude ×9".into() };
            db.group_samples(t, &[agents]).unwrap();
        }
        app.charts = Charts::Mem;
        app.load().unwrap();
        let s = screen(&mut app, 160, 40);
        assert!(s.contains('━'), "a line marks the memory in use:\n{s}");
        assert!(s.contains("programs hold ") && s.contains("compressed or in swap"), "{s}");
        let agent_rows = s.lines().filter(|l| l.contains('▒')).count();
        assert!(agent_rows >= 10, "the agents keep their full height:\n{s}");
    }

    #[test]
    fn filters_hide_other_namespaces() {
        let (_t, mut app) = fixture();
        app.ns_filter = Some("nothing-here".into());
        app.load().unwrap();
        let s = screen(&mut app, 150, 40);
        assert!(s.contains("no taskguard jobs in this range"), "{s}");
    }

    #[test]
    fn runs_view_explains_the_wait() {
        let (_t, mut app) = fixture();
        app.view = View::Runs;
        app.load().unwrap();
        let s = screen(&mut app, 150, 30);
        assert!(s.contains("packages/api:vitest_run"), "{s}");
        assert!(s.contains("STARVED"));
        assert!(s.contains("why it waited: 12s memory"), "{s}");
        assert!(s.contains("possibly starved: waited on CPU 33%"));
    }

    #[test]
    fn job_and_warnings_views() {
        let (_t, mut app) = fixture();
        app.job_key = Some("packages/api:vitest_run".into());
        app.view = View::Job;
        app.load().unwrap();
        let s = screen(&mut app, 150, 40);
        assert!(s.contains("needs CPU:    6.0 cores (learned"), "{s}");
        assert!(s.contains("next run reserves 6.0 cores (was 3.0)"), "{s}");
        app.view = View::Warnings;
        let s = screen(&mut app, 150, 40);
        assert!(s.contains("possibly starved (cpu): packages/api:vitest_run"), "{s}");
        assert!(s.contains("taskguard changed: next run reserves 6.0 cores (was 3.0)"), "{s}");
    }

    #[test]
    fn finished_columns_do_not_change_between_refreshes() {
        let (_t, mut app) = fixture();
        app.range = 0; // 5 minutes, 100 columns of 3 s
        app.load().unwrap();
        let (from1, first) = (app.data.from, app.data.machine.clone());
        std::thread::sleep(std::time::Duration::from_millis(1100));
        app.load().unwrap();
        let shift = ((app.data.from - from1) / 3.0).round() as usize;
        // Every column that was finished before still holds the same value,
        // only moved left by whole columns.
        let finished = &first[shift..first.len() - 1];
        for (i, (was, now)) in finished.iter().zip(&app.data.machine).enumerate() {
            assert_eq!(was.cpu, now.cpu, "column {}", i + shift);
        }
        assert_eq!((app.data.from / 3.0).fract(), 0.0, "edges sit on whole multiples of the width");
    }

    /// One running and one waiting job, both owned by this test process.
    fn with_queue(app: &mut App, running_over: bool) {
        let q = crate::queue::Queue::open(&app.dir).unwrap();
        let pid = std::process::id() as i32;
        let base = crate::queue::Entry { pid, ns: "dalp".into(), known: true, version: Some("0.1.3".into()), ..Default::default() };
        let run = crate::queue::Entry {
            key: "packages/big:build".into(),
            started_at: Some(now() - 30.0),
            need_cpu: 2.0,
            need_mem_kb: 1 << 20,
            live_cpu: if running_over { 5.0 } else { 1.0 },
            live_mem_kb: if running_over { 3 << 20 } else { 1 << 19 },
            ..base.clone()
        };
        let wait = crate::queue::Entry {
            ticket: 2,
            key: "packages/small:tsc".into(),
            queued_at: now() - 5.0,
            need_cpu: 1.0,
            need_mem_kb: 1 << 19,
            ..base
        };
        q.write(&q.run_path(pid), &run).unwrap();
        q.write(&q.wait_path(&wait), &wait).unwrap();
        app.load().unwrap();
    }

    #[test]
    fn queue_lists_running_jobs_first_and_marks_overage() {
        let (_t, mut app) = fixture();
        with_queue(&mut app, true);
        app.view = View::Queue;
        let s = screen(&mut app, 150, 30);
        let (run, wait) = (s.find("packages/big:build").unwrap(), s.find("packages/small:tsc").unwrap());
        assert!(run < wait, "the running job is listed first:\n{s}");
        assert!(s.contains("over estimates"), "{s}");
        assert!(s.contains("uses more than its estimate"), "the first row is selected: the running job\n{s}");
        app.sel = 1;
        let s = screen(&mut app, 150, 30);
        assert!(s.contains("Why is packages/small:tsc waiting?"), "{s}");
    }

    #[test]
    fn enter_opens_a_job_and_esc_returns_to_the_same_row() {
        let (_t, mut app) = fixture();
        with_queue(&mut app, false);
        app.view = View::Queue;
        app.sel = 1;
        let press = |app: &mut App, code| {
            app.key(crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE));
        };
        press(&mut app, KeyCode::Enter);
        assert_eq!((app.view, app.job_key.as_deref()), (View::Job, Some("packages/small:tsc")));
        app.load().unwrap();
        let s = screen(&mut app, 150, 40);
        assert!(s.contains("WAITING for"), "a job that waits now shows it:\n{s}");
        press(&mut app, KeyCode::Backspace);
        assert_eq!((app.view, app.sel), (View::Queue, 1));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.view, View::Job, "j opens the last job again");
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.view, View::Queue);
    }

    #[test]
    fn a_running_job_shows_its_load_over_time() {
        let (_t, mut app) = fixture();
        let db = crate::db::Db::open_dir(&app.dir).unwrap();
        let pid = std::process::id() as i32;
        let id = db.insert_run(&NewRun { ns: "dalp", key: "packages/live:vitest", ..Default::default() }).unwrap();
        for i in 0..20 {
            // Wants 6 cores, gets 2: its threads wait twice as long as they run.
            db.job_sample(id, now() - 40.0 + i as f64 * 2.0, 2.0, 2.0, 6.0, (1 + i) << 18, 0.0).unwrap();
        }
        let q = crate::queue::Queue::open(&app.dir).unwrap();
        let e = crate::queue::Entry {
            pid,
            run_id: id,
            key: "packages/live:vitest".into(),
            started_at: Some(now() - 40.0),
            need_cpu: 6.0,
            need_mem_kb: 8 << 20,
            live_cpu: 2.0,
            live_mem_kb: 5 << 20,
            ..Default::default()
        };
        q.write(&q.run_path(pid), &e).unwrap();
        app.job_key = Some("packages/live:vitest".into());
        app.view = View::Job;
        app.load().unwrap();
        let s = screen(&mut app, 160, 40);
        assert!(s.contains("RUNNING for"), "{s}");
        assert!(s.contains("cores used") && s.contains("memory") && s.contains("needs 8.0 GB"), "{s}");
        assert!(s.contains("✗ CPU: its threads wait for a core 200%"), "{s}");
    }

    #[test]
    fn long_lists_say_how_many_rows_are_hidden() {
        let (_t, mut app) = fixture();
        let db = crate::db::Db::open_dir(&app.dir).unwrap();
        for i in 0..40 {
            let id = db.insert_run(&NewRun { ns: "dalp", key: &format!("k{i}"), ..Default::default() }).unwrap();
            db.mark_started(id, now() - 10.0, 0.0, None).unwrap();
            db.finish_run(id, &RunResult { ended_at: now(), measured: true, peak_mem_kb: 1, ..Default::default() }).unwrap();
        }
        app.view = View::Runs;
        app.load().unwrap();
        let s = screen(&mut app, 150, 30);
        assert!(s.contains("more below"), "{s}");
        app.sel = 35;
        let s = screen(&mut app, 150, 30);
        assert!(s.contains("more above"), "{s}");
    }

    #[test]
    fn clicks_on_tabs_and_footer_keys() {
        let (_t, mut app) = fixture();
        let _ = screen(&mut app, 150, 30);
        let (y, x, _, _) = *app.hits.iter().find(|h| h.3 == Click::Tab(View::Trends)).unwrap();
        app.mouse(crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left), x + 1, y);
        assert_eq!(app.view, View::Trends);
        let _ = screen(&mut app, 150, 30);
        let (y, x, _, _) = *app.hits.iter().find(|h| h.3 == Click::Key(KeyCode::Char(' '))).unwrap();
        app.mouse(crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left), x, y);
        assert!(app.paused, "a click on 'space pause' pauses");
    }

    #[test]
    fn every_view_renders_on_a_small_screen() {
        let (_t, mut app) = fixture();
        app.job_key = Some("packages/api:vitest_run".into());
        for v in VIEWS.iter().map(|(v, _)| *v).chain([View::Job]) {
            app.view = v;
            let _ = screen(&mut app, 60, 12);
        }
    }
}
