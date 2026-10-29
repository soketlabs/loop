//! Small drawing helpers and number formatting shared by the views.

use super::*;

/// A table column. A leading `>` in `header` right-aligns it; on narrow terminals the
/// columns with the lowest `keep` are dropped first.
#[derive(Debug, Clone, Copy)]
pub(super) struct Column {
    pub(super) header: &'static str,
    pub(super) width: u16,
    pub(super) keep: u8,
    /// Takes the remaining width.
    pub(super) fill: bool,
}

impl Column {
    pub(super) const fn new(header: &'static str, width: u16, keep: u8) -> Self {
        Self {
            header,
            width,
            keep,
            fill: false,
        }
    }

    /// The identifying first column; never dropped.
    pub(super) const fn text(header: &'static str, width: u16) -> Self {
        Self::new(header, width, u8::MAX)
    }
}

/// Indices of the columns to show in a table `width` cells wide.
pub(super) fn fit_columns(columns: &[Column], width: u16) -> Vec<usize> {
    // Borders and the selection marker.
    let budget = width.saturating_sub(4);
    let needed = |kept: &[usize]| {
        kept.iter().map(|&i| columns[i].width).sum::<u16>()
            + 2 * kept.len().saturating_sub(1) as u16
    };
    let mut kept: Vec<usize> = (0..columns.len()).collect();
    while needed(&kept) > budget {
        let droppable = kept
            .iter()
            .enumerate()
            .filter(|(_, &i)| columns[i].keep < u8::MAX)
            .min_by_key(|(_, &i)| columns[i].keep);
        match droppable {
            Some((pos, _)) => {
                kept.remove(pos);
            }
            None => break,
        }
    }
    kept
}

/// One row of a ranked bar list.
pub(super) struct BarItem {
    pub(super) label: String,
    pub(super) value: f64,
    pub(super) value_text: String,
    pub(super) note: String,
}

impl BarItem {
    pub(super) fn new(label: &str, value: f64, value_text: String) -> Self {
        Self {
            label: label.to_string(),
            value,
            value_text,
            note: String::new(),
        }
    }

    pub(super) fn note(mut self, note: String) -> Self {
        self.note = note;
        self
    }
}

/// Width of a text column: its widest cell or header, capped.
pub(super) fn column_width<'a>(
    cells: impl Iterator<Item = &'a str>,
    header: &str,
    cap: usize,
) -> u16 {
    cells
        .map(|c| c.chars().count())
        .chain([header.len()])
        .max()
        .unwrap_or(0)
        .min(cap) as u16
}

/// A named daily series of a histogram.
pub(super) struct Series {
    pub(super) name: &'static str,
    /// The series' value on a day.
    pub(super) value: fn(&DayPoint) -> f64,
}

/// One series of a histogram's legend.
#[derive(Debug, Clone, Copy)]
pub(super) struct Legend<'a> {
    pub(super) name: &'a str,
    pub(super) color: Color,
}

/// One stat tile: a label, a big value and an optional note under it.
pub(super) struct Tile {
    pub(super) label: &'static str,
    pub(super) value: String,
    pub(super) note: String,
}

impl Tile {
    pub(super) fn new(label: &'static str, value: String, note: String) -> Self {
        Self { label, value, note }
    }
}

/// What [`App::histogram`] draws.
pub(super) struct Histogram<'a> {
    pub(super) title: &'a str,
    /// Y axis name, e.g. `Tokens`.
    pub(super) y_name: &'a str,
    /// X axis name, e.g. `Day`.
    pub(super) x_name: &'a str,
    /// Series names and colors, stacked bottom to top.
    pub(super) series: &'a [Legend<'a>],
    /// Per bar, one value per series.
    pub(super) bars: &'a [Vec<u64>],
    /// Per bar, its x label.
    pub(super) labels: &'a [String],
    /// Bars whose label is shown.
    pub(super) ticks: &'a [usize],
}

/// One day of the daily charts, including idle days.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DayPoint {
    pub(super) label: String,
    pub(super) input: u64,
    pub(super) output: u64,
    pub(super) llm_calls: u64,
    pub(super) tool_calls: u64,
    pub(super) runs: u64,
    pub(super) sub_agent_runs: u64,
}

/// Every day of the period, ending today, with zeros for idle days. `All` starts at the
/// first day with data.
pub(super) fn fill_days(days: &[DayRow], range: Range, today: NaiveDate) -> Vec<DayPoint> {
    let span = match range.days() {
        Some(d) => i64::from(d),
        None => days
            .first()
            .and_then(|d| NaiveDate::parse_from_str(&d.date, "%Y-%m-%d").ok())
            .map_or(1, |first| (today - first).num_days() + 1)
            .max(1),
    };
    (0..span)
        .rev()
        .map(|back| {
            let date = today - chrono::Duration::days(back);
            let key = date.format("%Y-%m-%d").to_string();
            let row = days
                .iter()
                .find(|d| d.date == key)
                .cloned()
                .unwrap_or_default();
            DayPoint {
                label: date.format("%b %-d").to_string(),
                input: row.input_tokens,
                output: row.output_tokens,
                llm_calls: row.llm_calls,
                tool_calls: row.tool_calls,
                runs: row.runs,
                sub_agent_runs: row.sub_agent_runs,
            }
        })
        .collect()
}

pub(super) fn weighted_latency(summary: &Summary) -> f64 {
    let calls: u64 = summary.models.iter().map(|m| m.calls).sum();
    if calls == 0 {
        return 0.0;
    }
    summary
        .models
        .iter()
        .map(|m| m.avg_latency_ms * m.calls as f64)
        .sum::<f64>()
        / calls as f64
}

pub(super) fn centered_rows(area: Rect, rows: u16) -> Rect {
    let h = rows.min(area.height);
    Rect {
        y: area.y + (area.height - h) / 2,
        height: h,
        ..area
    }
}

pub(super) fn right(text: String) -> Cell<'static> {
    Cell::from(Line::from(text).alignment(Alignment::Right))
}

/// Horizontal bar of `fraction` (0–1) of `width` cells, in eighths.
pub(super) fn bar(fraction: f64, width: usize) -> String {
    const PARTS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (fraction.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let mut out = "█".repeat(eighths / 8);
    out.push_str(PARTS[eighths % 8]);
    if out.is_empty() && fraction > 0.0 && width > 0 {
        out.push('▏');
    }
    out
}

/// Full-width gauge: filled part, then a light track.
pub(super) fn gauge(fraction: f64, width: usize) -> String {
    let filled = bar(fraction, width);
    let used = filled.chars().count();
    format!("{filled}{}", "░".repeat(width.saturating_sub(used)))
}

pub(super) fn nice_max(v: f64) -> f64 {
    if v <= 0.0 {
        return 1.0;
    }
    let p = 10f64.powf(v.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .map(|m| m * p)
        .find(|c| *c >= v)
        .unwrap_or(10.0 * p)
}

pub(super) fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `950`, `12.3K`, `4.5M`, `1.2B`.
pub fn compact(n: u64) -> String {
    compact_f(n as f64)
}

pub(super) fn compact_f(n: f64) -> String {
    let (value, unit) = match n {
        n if n >= 1e9 => (n / 1e9, "B"),
        n if n >= 1e6 => (n / 1e6, "M"),
        n if n >= 1e3 => (n / 1e3, "K"),
        n => return format!("{}", n.round() as u64),
    };
    let text = if value >= 100.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    };
    format!("{}{unit}", text.trim_end_matches(".0"))
}

/// `1,234,567`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `850ms`, `4.2s`, `3m 05s`.
pub fn duration(ms: f64) -> String {
    match ms {
        ms if ms < 1000.0 => format!("{}ms", ms.round() as u64),
        ms if ms < 60_000.0 => format!("{:.1}s", ms / 1000.0),
        ms => {
            let s = (ms / 1000.0).round() as u64;
            format!("{}m {:02}s", s / 60, s % 60)
        }
    }
}

/// `just now`, `5m ago`, `3h ago`, `2d ago`.
pub(super) fn ago(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - at).num_seconds().max(0);
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}
