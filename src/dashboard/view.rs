use std::time::SystemTime;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, Block, Borders, Chart, Clear, Dataset, Gauge, GraphType, Paragraph, Sparkline, Wrap,
    },
};

use crate::{
    app::{MetricsFetchStatus, MetricsRange, MetricsSnapshot},
    error::ApplicationError,
    output::escape_terminal_text,
};

use super::model::{DashboardModel, MetricKey, MetricSpec, MetricUnit, format_value, metric_spec};

pub(crate) enum DashboardData<'a> {
    Snapshot(&'a MetricsSnapshot),
    Error(&'a ApplicationError),
}

pub(crate) struct DashboardView<'a> {
    pub(crate) cluster_id: &'a str,
    pub(crate) data: DashboardData<'a>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LayoutMode {
    Full,
    Compact,
    Narrow,
}

pub(crate) fn render(frame: &mut Frame<'_>, view: DashboardView<'_>) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    frame.render_widget(Clear, area);
    let [header_area, body_area, footer_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    render_header(frame, header_area, &view);
    render_footer(frame, footer_area);

    match view.data {
        DashboardData::Error(error) => render_error(frame, body_area, error),
        DashboardData::Snapshot(snapshot) => {
            let model = DashboardModel::new(snapshot);
            match layout_mode(area) {
                LayoutMode::Full => render_full(frame, body_area, &model),
                LayoutMode::Compact => render_compact(frame, body_area, &model),
                LayoutMode::Narrow => render_narrow(frame, body_area, &model),
            }
        }
    }
}

fn layout_mode(area: Rect) -> LayoutMode {
    if area.width < 80 || area.height < 22 {
        LayoutMode::Narrow
    } else if area.width < 120 || area.height < 38 {
        LayoutMode::Compact
    } else {
        LayoutMode::Full
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, view: &DashboardView<'_>) {
    let cluster = escape_terminal_text(view.cluster_id);
    let (range, status, fetched_at) = match view.data {
        DashboardData::Snapshot(snapshot) => (
            range_label(snapshot.range),
            status_label(snapshot.status),
            fetched_at_label(snapshot.fetched_at),
        ),
        DashboardData::Error(_) => ("-", "Unavailable", "Fetch failed".to_owned()),
    };
    let style = match view.data {
        DashboardData::Snapshot(snapshot) => status_style(snapshot.status),
        DashboardData::Error(_) => Style::default().fg(Color::Red),
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " Aurora DSQL metrics ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{cluster}  {range}  ")),
            Span::styled(status, style),
            Span::raw(format!("  {fetched_at}")),
        ]))
        .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new("q/Esc return  r refresh  1 15m  2 1h  3 6h  4 24h")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn render_error(frame: &mut Frame<'_>, area: Rect, error: &ApplicationError) {
    frame.render_widget(
        Paragraph::new(escape_terminal_text(&error.to_string()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Metrics unavailable ")
                    .border_style(Style::default().fg(Color::Red)),
            )
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        area,
    );
}

const SUMMARY_KEYS: [MetricKey; 6] = [
    MetricKey::TotalTransactions,
    MetricKey::ReadOnlyTransactions,
    MetricKey::CommitLatency,
    MetricKey::OccConflicts,
    MetricKey::QueryTimeouts,
    MetricKey::ActiveConnections,
];

const DETAIL_KEYS: [MetricKey; 12] = [
    MetricKey::ReadOnlyTransactions,
    MetricKey::TotalDpu,
    MetricKey::ReadDpu,
    MetricKey::WriteDpu,
    MetricKey::ComputeDpu,
    MetricKey::MultiRegionWriteDpu,
    MetricKey::BytesRead,
    MetricKey::BytesWritten,
    MetricKey::ComputeTime,
    MetricKey::ClusterStorageSize,
    MetricKey::AdminConnectionAttempts,
    MetricKey::CustomRoleConnectionAttempts,
];

fn render_full(frame: &mut Frame<'_>, area: Rect, model: &DashboardModel<'_>) {
    let [summary_area, charts_area, details_area] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(10),
        Constraint::Min(0),
    ])
    .areas(area);
    render_summary(frame, summary_area, model);
    let [transactions_area, reliability_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(charts_area);
    render_chart(
        frame,
        transactions_area,
        "Transaction activity",
        &[
            (MetricKey::TotalTransactions, Color::Cyan),
            (MetricKey::ReadOnlyTransactions, Color::Green),
        ],
        model,
    );
    render_chart(
        frame,
        reliability_area,
        "Reliability",
        &[
            (MetricKey::OccConflicts, Color::Yellow),
            (MetricKey::QueryTimeouts, Color::Red),
        ],
        model,
    );
    render_metric_grid(frame, details_area, &DETAIL_KEYS, 2, model);
}

fn render_compact(frame: &mut Frame<'_>, area: Rect, model: &DashboardModel<'_>) {
    let [ratio_area, metrics_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
    render_ratio_gauge(frame, ratio_area, model);
    render_metric_grid(
        frame,
        metrics_area,
        &[
            MetricKey::TotalTransactions,
            MetricKey::ReadOnlyTransactions,
            MetricKey::CommitLatency,
            MetricKey::OccConflicts,
            MetricKey::QueryTimeouts,
            MetricKey::TotalDpu,
            MetricKey::ReadDpu,
            MetricKey::WriteDpu,
            MetricKey::ComputeDpu,
            MetricKey::MultiRegionWriteDpu,
            MetricKey::BytesRead,
            MetricKey::BytesWritten,
            MetricKey::ComputeTime,
            MetricKey::ClusterStorageSize,
            MetricKey::ActiveConnections,
            MetricKey::AdminConnectionAttempts,
            MetricKey::CustomRoleConnectionAttempts,
        ],
        3,
        model,
    );
}

fn render_narrow(frame: &mut Frame<'_>, area: Rect, model: &DashboardModel<'_>) {
    let [hint_area, cards_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new("Use a larger terminal for charts and all metrics")
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL)),
        hint_area,
    );
    let mut rows = Vec::with_capacity(SUMMARY_KEYS.len() + 1);
    rows.push((
        "Read-only ratio",
        format_value(MetricUnit::Percent, model.latest_read_only_ratio()),
    ));
    rows.extend(SUMMARY_KEYS.iter().map(|key| {
        let spec = metric_spec(*key);
        (spec.label, format_value(spec.unit, model.latest(*key)))
    }));
    let constraints = vec![Constraint::Length(2); rows.len()];
    for (area, (label, value)) in Layout::vertical(constraints)
        .split(cards_area)
        .iter()
        .zip(rows)
    {
        frame.render_widget(
            Paragraph::new(value).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {label} ")),
            ),
            *area,
        );
    }
}

fn render_summary(frame: &mut Frame<'_>, area: Rect, model: &DashboardModel<'_>) {
    let areas = Layout::horizontal([Constraint::Ratio(1, 7); 7]).split(area);
    render_metric_card(frame, areas[0], metric_spec(SUMMARY_KEYS[0]), model);
    render_ratio_gauge(frame, areas[1], model);
    for (area, key) in areas[2..].iter().zip(SUMMARY_KEYS[1..].iter()) {
        render_metric_card(frame, *area, metric_spec(*key), model);
    }
}

fn render_metric_card(
    frame: &mut Frame<'_>,
    area: Rect,
    spec: &MetricSpec,
    model: &DashboardModel<'_>,
) {
    frame.render_widget(
        Paragraph::new(format_value(spec.unit, model.latest(spec.key)))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", spec.label)),
            ),
        area,
    );
}

fn render_ratio_gauge(frame: &mut Frame<'_>, area: Rect, model: &DashboardModel<'_>) {
    let ratio = model.latest_read_only_ratio();
    let label = format_value(MetricUnit::Percent, ratio);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Read-only ratio ");
    let Some(ratio) = ratio else {
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Center)
                .block(block),
            area,
        );
        return;
    };
    frame.render_widget(
        Gauge::default()
            .block(block)
            .gauge_style(Style::default().fg(Color::Green))
            .percent(ratio.clamp(0.0, 100.0).round() as u16)
            .label(label),
        area,
    );
}

fn render_metric_grid(
    frame: &mut Frame<'_>,
    area: Rect,
    keys: &[MetricKey],
    columns: usize,
    model: &DashboardModel<'_>,
) {
    if area.is_empty() || columns == 0 {
        return;
    }
    let rows = keys.len().div_ceil(columns);
    let row_areas = Layout::vertical(vec![Constraint::Ratio(1, rows as u32); rows]).split(area);
    for (row_index, row_area) in row_areas.iter().enumerate() {
        let column_areas = Layout::horizontal(vec![Constraint::Ratio(1, columns as u32); columns])
            .split(*row_area);
        for (column_index, column_area) in column_areas.iter().enumerate() {
            let index = row_index * columns + column_index;
            if let Some(key) = keys.get(index) {
                render_sparkline(frame, *column_area, metric_spec(*key), model);
            }
        }
    }
}

fn render_sparkline(
    frame: &mut Frame<'_>,
    area: Rect,
    spec: &MetricSpec,
    model: &DashboardModel<'_>,
) {
    let data = model.sparkline_data(spec.key);
    let value = format_value(spec.unit, model.latest(spec.key));
    frame.render_widget(
        Sparkline::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} - {value} ", spec.label)),
            )
            .data(data)
            .max(100)
            .style(Style::default().fg(Color::Cyan))
            .absent_value_symbol(symbols::shade::MEDIUM)
            .absent_value_style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn render_chart(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    keys: &[(MetricKey, Color)],
    model: &DashboardModel<'_>,
) {
    let all_segments = keys
        .iter()
        .map(|(key, _)| model.chart_segments(*key))
        .collect::<Vec<_>>();
    let max_x = all_segments
        .iter()
        .flat_map(|segments| segments.iter().flatten())
        .map(|(x, _)| *x)
        .fold(1.0_f64, f64::max);
    let max_y = all_segments
        .iter()
        .flat_map(|segments| segments.iter().flatten())
        .map(|(_, y)| *y)
        .fold(1.0_f64, f64::max);
    let mut datasets = Vec::new();
    for ((key, color), segments) in keys.iter().zip(all_segments.iter()) {
        let spec = metric_spec(*key);
        for (index, segment) in segments.iter().enumerate() {
            let dataset = Dataset::default()
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(*color))
                .data(segment);
            datasets.push(if index == 0 {
                dataset.name(spec.label)
            } else {
                dataset
            });
        }
    }
    frame.render_widget(
        Chart::new(datasets)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {title} ")),
            )
            .x_axis(Axis::default().bounds([0.0, max_x]))
            .y_axis(Axis::default().bounds([0.0, max_y * 1.05])),
        area,
    );
}

fn range_label(range: MetricsRange) -> &'static str {
    match range {
        MetricsRange::FifteenMinutes => "15 minutes",
        MetricsRange::OneHour => "1 hour",
        MetricsRange::SixHours => "6 hours",
        MetricsRange::TwentyFourHours => "24 hours",
    }
}

fn status_label(status: MetricsFetchStatus) -> &'static str {
    match status {
        MetricsFetchStatus::Fresh => "Fresh",
        MetricsFetchStatus::Stale => "Stale",
        MetricsFetchStatus::Unavailable => "Unavailable",
    }
}

fn status_style(status: MetricsFetchStatus) -> Style {
    let color = match status {
        MetricsFetchStatus::Fresh => Color::Green,
        MetricsFetchStatus::Stale => Color::Yellow,
        MetricsFetchStatus::Unavailable => Color::Red,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn fetched_at_label(fetched_at: Option<SystemTime>) -> String {
    fetched_at
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| format!("Fetched at Unix {}", duration.as_secs()))
        .unwrap_or_else(|| "Not fetched".to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use ratatui::{Terminal, backend::TestBackend};

    use crate::{
        app::{MetricSeries, MetricsFetchStatus, MetricsRange, MetricsSnapshot},
        error::ApplicationError,
    };

    use super::{DashboardData, DashboardView, render};

    const REQUIRED_LABELS: [&str; 18] = [
        "Total transactions",
        "Read-only transactions",
        "Read-only ratio",
        "Commit latency",
        "OCC conflicts",
        "Query timeouts",
        "Total DPU",
        "Read DPU",
        "Write DPU",
        "Compute DPU",
        "Multi-Region write DPU",
        "Bytes read",
        "Bytes written",
        "Compute time",
        "Cluster storage",
        "Active connections",
        "Admin connection attempts",
        "Custom-role connection attempts",
    ];

    fn populated_snapshot() -> MetricsSnapshot {
        let metric_ids = [
            "total_transactions",
            "read_only_transactions",
            "commit_latency",
            "occ_conflicts",
            "query_timeouts",
            "total_dpu",
            "read_dpu",
            "write_dpu",
            "compute_dpu",
            "multi_region_write_dpu",
            "bytes_read",
            "bytes_written",
            "compute_time",
            "cluster_storage_size",
            "active_connections",
            "admin_connection_attempts",
            "custom_role_connection_attempts",
        ];
        MetricsSnapshot {
            range: MetricsRange::OneHour,
            fetched_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000)),
            series: metric_ids
                .iter()
                .enumerate()
                .map(|(index, metric)| MetricSeries {
                    metric: (*metric).to_owned(),
                    samples: vec![Some(index as f64 + 1.0), None, Some(index as f64 + 2.0)],
                })
                .collect(),
            status: MetricsFetchStatus::Fresh,
        }
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in buffer.area.top()..buffer.area.bottom() {
            for x in buffer.area.left()..buffer.area.right() {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    fn draw(terminal: &mut Terminal<TestBackend>, cluster_id: &str, data: DashboardData<'_>) {
        terminal
            .draw(|frame| {
                render(frame, DashboardView { cluster_id, data });
            })
            .expect("test backend draw should succeed");
    }

    #[test]
    fn normal_wide_snapshot_contains_every_required_widget_and_visualization_kind() {
        let snapshot = populated_snapshot();
        let mut terminal = Terminal::new(TestBackend::new(150, 48)).unwrap();

        draw(&mut terminal, "abc123", DashboardData::Snapshot(&snapshot));

        let text = buffer_text(&terminal);
        for label in REQUIRED_LABELS {
            assert!(text.contains(label), "missing widget label {label:?}");
        }
        assert!(text.contains("Transaction activity"));
        assert!(text.contains("Reliability"));
        assert!(text.contains("Fresh"));
        assert!(text.contains("1 hour"));
        assert!(!text.contains("Use a larger terminal"));
    }

    #[test]
    fn no_data_snapshot_renders_no_data_instead_of_numeric_zeroes() {
        let snapshot = MetricsSnapshot {
            range: MetricsRange::FifteenMinutes,
            fetched_at: Some(SystemTime::UNIX_EPOCH),
            series: Vec::new(),
            status: MetricsFetchStatus::Fresh,
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

        draw(&mut terminal, "cluster", DashboardData::Snapshot(&snapshot));

        let text = buffer_text(&terminal);
        assert!(text.contains("No data"));
        assert!(!text.contains("0 DPU"));
        assert!(!text.contains("0.0%"));
    }

    #[test]
    fn wide_no_data_snapshot_does_not_present_the_ratio_as_zero() {
        let snapshot = MetricsSnapshot {
            range: MetricsRange::OneHour,
            fetched_at: None,
            series: Vec::new(),
            status: MetricsFetchStatus::Fresh,
        };
        let mut terminal = Terminal::new(TestBackend::new(150, 48)).unwrap();

        draw(&mut terminal, "cluster", DashboardData::Snapshot(&snapshot));

        let text = buffer_text(&terminal);
        assert!(text.contains("Read-only ratio"));
        assert!(text.contains("No data"));
        assert!(!text.contains("0.0%"));
    }

    #[test]
    fn error_snapshot_escapes_terminal_controls() {
        let error = ApplicationError::runtime(
            "CloudWatch metrics are unavailable; allow cloudwatch:GetMetricData on *\u{1b}[31m\ntry again",
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();

        draw(
            &mut terminal,
            "cluster\u{1b}]0;bad",
            DashboardData::Error(&error),
        );

        let text = buffer_text(&terminal);
        assert!(text.contains("Metrics unavailable"));
        assert!(
            text.contains("cloudwatch:GetMetricData on *"),
            "rendered error did not retain the IAM action: {text:?}"
        );
        assert!(text.contains("\\u{001b}[31m\\ntry again"));
        assert!(text.contains("cluster\\u{001b}]0;bad"));
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn narrow_snapshot_degrades_to_summary_cards_with_a_size_hint() {
        let snapshot = populated_snapshot();
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();

        draw(&mut terminal, "cluster", DashboardData::Snapshot(&snapshot));

        let text = buffer_text(&terminal);
        assert!(text.contains("Use a larger terminal for charts and all metrics"));
        assert!(text.contains("Total transactions"));
        assert!(text.contains("Read-only ratio"));
        assert!(!text.contains("Transaction activity"));
    }

    #[test]
    fn compact_snapshot_keeps_every_required_metric_and_ratio() {
        let snapshot = populated_snapshot();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

        draw(&mut terminal, "cluster", DashboardData::Snapshot(&snapshot));

        let text = buffer_text(&terminal);
        for label in REQUIRED_LABELS {
            assert!(
                text.contains(label),
                "missing compact widget label {label:?}"
            );
        }
    }

    #[test]
    fn tiny_terminal_sizes_render_without_panicking() {
        let snapshot = populated_snapshot();
        for (width, height) in [(1, 1), (2, 3), (10, 4), (20, 8)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            draw(&mut terminal, "cluster", DashboardData::Snapshot(&snapshot));
        }
    }

    #[test]
    fn resized_snapshot_recomputes_layout_without_retaining_narrow_state() {
        let snapshot = populated_snapshot();
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();
        draw(&mut terminal, "cluster", DashboardData::Snapshot(&snapshot));
        assert!(buffer_text(&terminal).contains("Use a larger terminal"));

        terminal.backend_mut().resize(150, 48);
        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 150, 48))
            .unwrap();
        draw(&mut terminal, "cluster", DashboardData::Snapshot(&snapshot));

        let text = buffer_text(&terminal);
        assert!(!text.contains("Use a larger terminal"));
        assert!(text.contains("Transaction activity"));
        assert!(text.contains("Multi-Region write DPU"));
    }
}
