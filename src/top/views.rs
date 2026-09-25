//! Drawing the dashboard's views.

use super::chart::{self, AXIS_W, PALETTE, Stacked, WaitStrip, spark};
use super::{App, Charts, Input, NS_SORTS, RANGES, RUN_SORTS, VIEWS, View};
use crate::dash::{self, Marker};
use crate::report::{self, chrono_like, datetime, dur, gb};
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
            format!("⚠ {} warnings (6)", d.warnings.len()),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    for (i, (v, name)) in VIEWS.iter().enumerate() {
        let style = if *v == app.view {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else if *v == View::Job && app.job_key.is_none() {
            DIM
        } else {
            Style::default()
        };
        spans.push(Span::styled(format!(" {} {} ", i + 1, name), style));
        spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let text = match &app.input {
        Input::Filter(t) => format!("filter keys: {t}▏   Enter apply, Esc cancel"),
        Input::ConfirmKill(pid, key) => format!("send SIGTERM to {key} (pid {pid})? y / n"),
        Input::None => match &app.message {
            Some(m) => m.clone(),
            None => {
                let common = "q quit  1-8/Tab views  t range  n namespace  / filter  Esc clear  space pause";
                let extra = match app.view {
                    View::Overview => "  ←/→ time cursor  c/m/b charts  o other layer",
                    View::Queue => "  ↑/↓ select  Enter job  k stop job",
                    View::Runs => {
                        return f.render_widget(
                            Paragraph::new(format!(
                                "{common}  ↑/↓ select  Enter job  s sort ({})  r reverse",
                                RUN_SORTS[app.sort % RUN_SORTS.len()]
                            ))
                            .style(DIM),
                            area,
                        );
                    }
                    View::Namespaces => {
                        return f.render_widget(
                            Paragraph::new(format!("{common}  s sort ({})  r reverse  Enter top job", NS_SORTS[app.sort % NS_SORTS.len()]))
                                .style(DIM),
                            area,
                        );
                    }
                    View::Trends | View::Warnings => "  ↑/↓ select  Enter job",
                    View::Job => "  Esc back",
                    View::Help => "",
                };
                format!("{common}{extra}")
            }
        },
    };
    f.render_widget(Paragraph::new(text).style(DIM), area);
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
        let fmt_cpu = |v: f64| format!("{v:.0}c");
        let fmt_mem = |v: f64| if v <= 0.0 { "0G".into() } else { gb(v as u64).replace(" GB", "G").replace(" MB", "M") };
        let (max, limit, fmt, title): (f64, f64, &dyn Fn(f64) -> String, &str) = if is_cpu {
            (d.ncpu as f64, d.ncpu as f64 * app.cfg.cpu_max / 100.0, &fmt_cpu, "CPU (cores)")
        } else {
            (d.mem_total_kb, d.mem_total_kb * app.cfg.mem_max / 100.0, &fmt_mem, "Memory")
        };
        f.render_widget(
            Stacked { total: &total, layers: &layers, max, limit, show_other: app.show_other, cursor: app.cursor, fmt, title },
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
        lines.push(Line::from(vec![
            Span::styled("░░ ", DIM),
            Span::raw(format!(
                "{:<14} {:>4.1}c {:>8}",
                "other",
                (machine_at.cpu - ours.0).max(0.0),
                gb((machine_at.mem_kb - ours.1).max(0.0) as u64)
            )),
        ]));
        lines.push(Line::styled("   (not started by taskguard)", DIM));
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

// ----------------------------------------------------------------- queue ----

fn queue(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(s) = app.data.snap.as_ref() else {
        f.render_widget(Paragraph::new("no queue data"), area);
        return;
    };
    let m = &s.machine;
    let bars = vec![
        Line::raw(format!(
            "CPU     [{}]  {:>5.1} busy  +{:.1} promised to running jobs  of {:.1} cores  limit {:.0}%",
            report::bar(m.cpu_busy, s.reserve_cpu, m.ncpu as f64, s.cpu_limit, 24),
            m.cpu_busy,
            s.reserve_cpu,
            m.ncpu as f64,
            s.limits.cpu_max_pct
        )),
        Line::raw(format!(
            "MEMORY  [{}]  {} held  +{} promised  of {}  limit {:.0}%",
            report::bar(m.mem_used_kb as f64, s.reserve_mem_kb as f64, m.mem_total_kb as f64, s.mem_limit_kb as f64, 24),
            gb(m.mem_used_kb),
            gb(s.reserve_mem_kb),
            gb(m.mem_total_kb),
            s.limits.mem_max_pct
        )),
        Line::styled("         # in use now   + promised to running jobs   | limit", DIM),
    ];
    let n_rows = s.waiting.len() + s.running.len();
    let [top, list, why] =
        Layout::vertical([Constraint::Length(4), Constraint::Length((n_rows as u16 + 2).clamp(3, 14)), Constraint::Min(6)]).areas(area);
    f.render_widget(Paragraph::new(bars), top);

    let mut rows: Vec<Row> = Vec::new();
    for w in &s.waiting {
        let e = &w.entry;
        let blocked = report::main_blocker(&w.decision).map(|b| b.name().to_uppercase()).unwrap_or_else(|| "starting".into());
        rows.push(Row::new(vec![
            Cell::from("WAIT").style(Style::default().fg(Color::Yellow)),
            Cell::from(trunc(&e.key, 44)),
            Cell::from(e.ns.clone()),
            Cell::from(dur(s.now - e.queued_at)),
            Cell::from(format!("{:.1}c {}{}", e.need_cpu, gb(e.need_mem_kb), if e.known || e.raised_by_min { "" } else { " est" })),
            Cell::from(blocked)
                .style(Style::default().fg(chart::blocker_color(report::main_blocker(&w.decision).map(|b| b.name()).unwrap_or("")))),
        ]));
    }
    for e in &s.running {
        rows.push(Row::new(vec![
            Cell::from(if e.now { "NOW" } else { "RUN" }).style(Style::default().fg(Color::Green)),
            Cell::from(trunc(&e.key, 44)),
            Cell::from(e.ns.clone()),
            Cell::from(dur(s.now - e.started_at.unwrap_or(s.now))),
            Cell::from(format!("{:.1}c {} now", e.live_cpu, gb(e.live_mem_kb))),
            Cell::from(format!("needs {:.1}c {}{}", e.need_cpu, gb(e.need_mem_kb), if e.known || e.raised_by_min { "" } else { " est" })),
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
    .block(Block::new().borders(Borders::TOP).title(format!(" {} waiting, {} running ", s.waiting.len(), s.running.len())));
    let mut st = table_state(app);
    f.render_stateful_widget(table, list, &mut st);
    remember_rows(app, list, 2, &st, n_rows);

    // The Why panel.
    let Some(s) = app.data.snap.as_ref() else { return };
    let mut lines: Vec<Line> = Vec::new();
    if let Some(w) = s.waiting.get(app.sel) {
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
            && let Some((name, mem, _)) = s.top_other.first()
        {
            lines.push(Line::raw(format!("  the biggest user outside taskguard is {name} ({})", gb(*mem))));
        }
        let since = e.blocker_since.map(|t| format!(" for {}", dur(s.now - t))).unwrap_or_default();
        lines.push(Line::raw(format!(
            "Waiting {} in total; the current reason has held{since}. {}",
            dur(s.now - e.queued_at),
            if e.bypassed_since.is_some() { "Newer jobs have passed it; after 2 minutes it gets a reservation." } else { "" }
        )));
        if !e.known && !e.raised_by_min {
            lines.push(Line::styled(
                format!(
                    "First run: nothing is learned yet, so it reserves an estimate ({}). The run teaches its real needs.",
                    e.estimate_from.as_deref().unwrap_or("a default")
                ),
                DIM,
            ));
        }
    } else if let Some((e, _)) = app.queue_row(app.sel) {
        lines.push(Line::styled(format!("{} is running", e.key), BOLD));
        lines.push(Line::raw(format!(
            "now {:.1} cores, {}; needs {:.1} cores, {} ({}); started {} ago",
            e.live_cpu,
            gb(e.live_mem_kb),
            e.need_cpu,
            gb(e.need_mem_kb),
            if e.known {
                "learned"
            } else if e.raised_by_min {
                "minimum"
            } else {
                "estimate"
            },
            dur(s.now - e.started_at.unwrap_or(s.now))
        )));
        if let Some(d) = e.est_dur_s {
            lines.push(Line::raw(format!("usually takes {}, so about {} left", dur(d), dur(d - (s.now - e.started_at.unwrap_or(s.now))))));
        }
        if e.now {
            lines.push(Line::raw("it skipped the queue (--now); it is measured like any other job"));
        }
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
    let mut st = table_state(app);
    f.render_stateful_widget(table, list, &mut st);
    remember_rows(app, list, 1, &st, n);

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
    let mut st = table_state(app);
    f.render_stateful_widget(t, list, &mut st);
    remember_rows(app, list, 2, &st, n);
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
    let mut st = table_state(app);
    f.render_stateful_widget(table, list, &mut st);
    remember_rows(app, list, 2, &st, n);
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
    let mut st = table_state(app);
    f.render_stateful_widget(table, area, &mut st);
    remember_rows(app, area, 2, &st, n);
}

// ------------------------------------------------------------------ help ----

fn help(f: &mut Frame, area: Rect) {
    let text = "\
VIEWS
  1 Overview    machine load over time, with taskguard's jobs stacked per namespace
  2 Queue       what runs and what waits, and a Why panel for the selected waiting job
  3 Runs        past runs; the panel below shows why the selected run waited
  4 Job         one command: its learned needs, their history, and what taskguard changed
  5 Trends      commands whose memory, CPU or duration grows
  6 Warnings    possibly starved runs, suggested minimums, trend alerts, --now runs over a limit
  7 Namespaces  load and waits per namespace
  8 Help        this page

KEYS
  q quit   1-8 / Tab views   t / T time range   n next namespace   / filter jobs   Esc clear or back
  ←/→ time cursor (Overview)   c / m / b CPU, memory or both charts   o hide or show the \"other\" layer
  ↑/↓ select   Enter open the job   s sort   r reverse   k stop a job (Queue)   space pause
  mouse: click a row to select it; scroll to change the time range

CHART LAYERS (Overview)
  ██ coloured   load of jobs taskguard started, one colour per namespace
  ░░ grey       the rest of the machine's load: things taskguard did not start
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

    #[test]
    fn every_view_renders_on_a_small_screen() {
        let (_t, mut app) = fixture();
        for (v, _) in VIEWS {
            app.view = v;
            let _ = screen(&mut app, 60, 12);
        }
    }
}
