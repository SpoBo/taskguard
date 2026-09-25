//! A stacked area chart made of terminal cells: the machine total at the
//! bottom as the backdrop, taskguard's load per namespace stacked on top of it
//! in colour, the rest marked as "other", and a line at the limit.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;

pub const PALETTE: [Color; 8] =
    [Color::Cyan, Color::Green, Color::Magenta, Color::Blue, Color::Yellow, Color::LightRed, Color::LightCyan, Color::LightGreen];

pub struct Stacked<'a> {
    /// Machine total per column.
    pub total: &'a [Option<f64>],
    /// Per namespace, the value per column, bottom first.
    pub layers: &'a [(String, Color, Vec<f64>)],
    /// Per group of programs outside taskguard, stacked above the namespaces.
    pub groups: &'a [(String, Color, Vec<f64>)],
    pub max: f64,
    pub limit: f64,
    pub show_other: bool,
    pub cursor: Option<usize>,
    /// Formats a y-axis value.
    pub fmt: &'a dyn Fn(f64) -> String,
    pub title: &'a str,
}

pub const AXIS_W: u16 = 8;

/// A fixed, muted colour per group of programs outside taskguard, apart from
/// the bright namespace colours.
pub fn group_color(name: &str) -> Color {
    match name {
        "agents" => Color::Indexed(141),
        "browsers" => Color::Indexed(75),
        "editors" => Color::Indexed(179),
        "dev services" => Color::Indexed(108),
        "containers" => Color::Indexed(247),
        "chat & apps" => Color::Indexed(174),
        _ => Color::Indexed(244),
    }
}

impl Widget for Stacked<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height < 3 || area.width <= AXIS_W + 2 {
            return;
        }
        let plot = Rect { x: area.x + AXIS_W, y: area.y + 1, width: area.width - AXIS_W, height: area.height - 1 };
        buf.set_string(area.x, area.y, self.title, Style::default().add_modifier(Modifier::BOLD));
        let rows = plot.height as usize;
        let max = if self.max > 0.0 { self.max } else { 1.0 };
        let per_row = max / rows as f64;
        // y-axis labels: top, middle, bottom.
        for (r, v) in [(0usize, max), (rows / 2, max / 2.0), (rows - 1, 0.0)] {
            let label = format!("{:>w$}", (self.fmt)(v), w = (AXIS_W - 1) as usize);
            buf.set_string(area.x, plot.y + r as u16, label, Style::default().fg(Color::DarkGray));
        }
        let limit_row = rows as f64 - (self.limit / per_row);
        for c in 0..plot.width as usize {
            let x = plot.x + c as u16;
            let Some(total) = self.total.get(c).copied().flatten() else {
                continue;
            };
            let mut stack: Vec<(f64, Style, &str)> = Vec::new();
            let mut acc = 0.0;
            for (_, color, vals) in self.layers {
                let v = vals.get(c).copied().unwrap_or(0.0);
                if v > 0.0 {
                    acc += v;
                    stack.push((acc, Style::default().fg(*color), "█"));
                }
            }
            if self.show_other {
                // Drawn in full, even above the machine total: a process's
                // memory also counts what the system compressed or swapped
                // out, so the programs can add up to more than is in use.
                for (_, color, vals) in self.groups {
                    let v = vals.get(c).copied().unwrap_or(0.0);
                    if v > 0.0 {
                        acc += v;
                        stack.push((acc, Style::default().fg(*color), "▒"));
                    }
                }
                if total > acc {
                    stack.push((total, Style::default().fg(Color::DarkGray), "░"));
                }
            }
            // Where the layers pass the machine total, a line marks it.
            let total_row = (acc > total * 1.02).then(|| rows as f64 - total / per_row);
            for r in 0..rows {
                // Value range this cell covers, bottom row first.
                let from_bottom = rows - 1 - r;
                let mid = (from_bottom as f64 + 0.5) * per_row;
                let y = plot.y + r as u16;
                if total_row.is_some_and(|t| (r as f64 - t).abs() < 0.5) {
                    buf.set_string(x, y, "━", Style::default().fg(Color::White));
                } else if let Some((_, style, sym)) = stack.iter().find(|(top, _, _)| mid <= *top) {
                    buf.set_string(x, y, *sym, *style);
                } else if (r as f64 - limit_row).abs() < 0.5 {
                    buf.set_string(x, y, "╌", Style::default().fg(Color::Yellow));
                }
            }
            if self.cursor == Some(c) {
                for r in 0..rows {
                    let cell = &mut buf[(x, plot.y + r as u16)];
                    cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }
}

/// A one-row strip: how many jobs waited per column, coloured by the most
/// common reason.
pub struct WaitStrip<'a> {
    pub waiting: &'a [(usize, String)],
    pub cursor: Option<usize>,
}

pub fn blocker_color(name: &str) -> Color {
    match name {
        "cpu" => Color::Red,
        "pressure" => Color::LightMagenta,
        "memory" => Color::Magenta,
        "slots" => Color::Yellow,
        "reserved" => Color::Blue,
        "learning" => Color::Cyan,
        _ => Color::Gray,
    }
}

impl Widget for WaitStrip<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width <= AXIS_W {
            return;
        }
        buf.set_string(area.x, area.y, "waiting", Style::default().fg(Color::DarkGray));
        let peak = self.waiting.iter().map(|w| w.0).max().unwrap_or(0).max(1);
        const LEVELS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
        for c in 0..(area.width - AXIS_W) as usize {
            let Some((n, why)) = self.waiting.get(c) else { break };
            if *n == 0 {
                continue;
            }
            let lvl = ((*n as f64 / peak as f64) * 7.0).round() as usize;
            let mut style = Style::default().fg(blocker_color(why));
            if self.cursor == Some(c) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            buf.set_string(area.x + AXIS_W + c as u16, area.y, LEVELS[lvl.min(7)], style);
        }
    }
}

/// Text sparkline for tables.
pub fn spark(values: &[f64], width: usize) -> String {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() {
        return String::new();
    }
    let v = &values[values.len().saturating_sub(width)..];
    let (lo, hi) = v.iter().fold((f64::MAX, f64::MIN), |(a, b), x| (a.min(*x), b.max(*x)));
    v.iter().map(|x| if hi > lo { LEVELS[(((x - lo) / (hi - lo)) * 7.0).round() as usize] } else { LEVELS[3] }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stacked_draws_layers_other_and_limit() {
        let total = vec![Some(8.0); 4];
        let layers = vec![("dalp".to_string(), Color::Cyan, vec![4.0; 4])];
        let fmt = |v: f64| format!("{v:.0}");
        let area = Rect::new(0, 0, AXIS_W + 4, 9);
        let mut buf = Buffer::empty(area);
        Stacked {
            total: &total,
            layers: &layers,
            groups: &[],
            max: 12.0,
            limit: 10.0,
            show_other: true,
            cursor: None,
            fmt: &fmt,
            title: "CPU",
        }
        .render(area, &mut buf);
        let col: Vec<String> = (1..9).map(|y| buf[(AXIS_W, y)].symbol().to_string()).collect();
        // 8 rows of 1.5 cores each: limit near 10, other up to 8, dalp up to 4.
        assert_eq!(col, vec![" ", "╌", " ", "░", "░", "█", "█", "█"].into_iter().map(String::from).collect::<Vec<_>>());
        assert_eq!(buf[(AXIS_W, 8)].fg, Color::Cyan);
    }

    #[test]
    fn sparklines() {
        assert_eq!(spark(&[1.0, 2.0, 3.0], 10), "▁▅█");
        assert_eq!(spark(&[5.0, 5.0], 10), "▄▄");
    }
}
