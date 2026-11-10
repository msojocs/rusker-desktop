//! Native, continuous-time container-stat charts. The caller owns sampling and history.
use chrono::{Local, TimeZone as _};
use gpui_kit as gpui;
use gpui_kit::base::StyledExt as _;
use gpui_kit::{
    canvas, component::ActiveTheme as _, div, fill, point, px, size, App, Bounds, Entity, Font,
    Hsla, InteractiveElement as _, IntoElement, ParentElement as _, PathBuilder, Pixels, Point,
    RenderOnce, StatefulInteractiveElement as _, Styled as _, TextAlign, TextRun, Window,
};

#[derive(Clone)]
pub struct ChartPoint {
    /// Elapsed seconds, not an evenly spaced category index.
    pub seconds: f64,
    /// Actual sampling time, used for local 24-hour clock labels.
    pub timestamp: i64,
    pub values: [Option<f64>; 2],
    /// Whether this sample may be joined to the immediately preceding sample.
    pub connected: bool,
}

#[derive(Clone, Copy)]
pub enum ChartUnit {
    Percent,
    Bytes,
}

/// `id` must be unique among charts in a view. `series` contains one or two
/// stable labels and their already-formatted current values, in blue/orange order.
/// Missing, negative and non-finite measurements are gaps, never invented zeroes.
pub fn stats_chart(
    id: &'static str,
    title: &'static str,
    subtitle: String,
    series: Vec<(&'static str, String)>,
    points: Vec<ChartPoint>,
    unit: ChartUnit,
    hover: Entity<StatsHover>,
) -> impl IntoElement {
    StatsChart {
        id,
        title,
        subtitle,
        series: series.into_iter().take(2).collect(),
        points,
        unit,
        hover,
    }
}

#[derive(IntoElement)]
struct StatsChart {
    id: &'static str,
    title: &'static str,
    subtitle: String,
    series: Vec<(&'static str, String)>,
    points: Vec<ChartPoint>,
    unit: ChartUnit,
    hover: Entity<StatsHover>,
}

/// Shared by all charts in one statistics view. All charts use the same
/// elapsed-time origin, which stays fixed while the view appends new points.
#[derive(Default)]
pub struct StatsHover {
    selected: Option<f64>,
    source: Option<&'static str>,
}

impl StatsHover {
    pub fn clear(&mut self) {
        self.selected = None;
        self.source = None;
    }
}

#[derive(Default)]
struct PlotGeometry {
    plot: Option<Bounds<Pixels>>,
}

impl RenderOnce for StatsChart {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(
            (gpui::ElementId::Name(self.id.into()), "stats-hover"),
            cx,
            |_, _| PlotGeometry::default(),
        );
        let colors: [Hsla; 2] = if cx.theme().mode.is_dark() {
            [gpui::rgb(0x3987e5).into(), gpui::rgb(0xd95926).into()]
        } else {
            [gpui::rgb(0x2a78d6).into(), gpui::rgb(0xeb6834).into()]
        };
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let grid = cx.theme().border.opacity(0.55);
        let surface: Hsla = gpui::rgb(if cx.theme().mode.is_dark() {
            0x1a1a19
        } else {
            0xfcfcfb
        })
        .into();
        let font = Font {
            family: cx.theme().font_family.clone(),
            ..Default::default()
        };
        let scale = Scale::new(&self.points, self.series.len(), self.unit);
        let hovered = self
            .hover
            .read(cx)
            .selected
            .and_then(|seconds| nearest_point(&self.points, seconds));
        let hover_times: Vec<f64> = self.points.iter().map(|sample| sample.seconds).collect();
        let labels: Vec<_> = self.series.iter().map(|(label, _)| *label).collect();
        let unit = self.unit;
        let points = self.points;
        let geometry_state = state.clone();
        let move_geometry = state;
        let move_state = self.hover.clone();
        let leave_state = self.hover;
        let chart_id = self.id;
        let border = cx.theme().border;

        div()
            .id(self.id)
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(surface)
            .text_color(foreground)
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(div().font_semibold().child(self.title))
                    .child(
                        div().flex().flex_wrap().gap_3().children(
                            self.series
                                .into_iter()
                                .enumerate()
                                .map(|(index, (label, value))| {
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .text_sm()
                                        .child(line_key(colors[index], index == 1))
                                        .child(div().text_color(muted).child(label))
                                        .child(div().font_semibold().child(value))
                                }),
                        ),
                    ),
            )
            .child(div().text_xs().text_color(muted).child(self.subtitle))
            .child(
                div()
                    .id("plot")
                    .w_full()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .on_mouse_move(move |event, _, cx| {
                        let selected = move_geometry.read(cx).plot.and_then(|plot| {
                            if !plot.contains(&event.position) {
                                return None;
                            }
                            let fraction =
                                f64::from((event.position.x - plot.left()) / plot.size.width);
                            nearest_time(
                                &hover_times,
                                scale.start + fraction * (scale.end - scale.start),
                            )
                        });
                        move_state.update(cx, |state, cx| {
                            if state.selected != selected
                                || (selected.is_some() && state.source != Some(chart_id))
                            {
                                state.selected = selected;
                                state.source = selected.map(|_| chart_id);
                                cx.notify();
                            }
                        });
                    })
                    .on_hover(move |inside, _, cx| {
                        if !inside {
                            leave_state.update(cx, |state, cx| {
                                // A late leave event from one chart must not clear another.
                                if state.source == Some(chart_id) {
                                    state.clear();
                                    cx.notify();
                                }
                            });
                        }
                    })
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                let plot = plot_bounds(bounds);
                                geometry_state.update(cx, |state, _| state.plot = Some(plot));
                                plot
                            },
                            move |bounds, plot, window, cx| {
                                if plot.size.width <= px(0.) || plot.size.height <= px(0.) {
                                    return;
                                }
                                let y_ticks = if plot.size.height < px(100.) { 2 } else { 4 };
                                for tick in 0..=y_ticks {
                                    let value = scale.ceiling * tick as f64 / y_ticks as f64;
                                    let y = plot.bottom()
                                        - plot.size.height * (tick as f32 / y_ticks as f32);
                                    stroke_line(
                                        point(plot.left(), y),
                                        point(plot.right(), y),
                                        grid,
                                        1.,
                                        window,
                                    );
                                    paint_text(
                                        axis_label(value, scale.ceiling, unit),
                                        point(bounds.left(), y - px(8.)),
                                        px(62.),
                                        TextAlign::Right,
                                        &font,
                                        muted,
                                        window,
                                        cx,
                                    );
                                }
                                // Geometry uses monotonic elapsed time; labels show actual local time.
                                let x_ticks = if plot.size.width < px(180.) { 1 } else { 2 };
                                for tick in 0..=x_ticks {
                                    let fraction = tick as f64 / x_ticks as f64;
                                    let seconds =
                                        scale.start + (scale.end - scale.start) * fraction;
                                    let x = plot.left() + plot.size.width * fraction as f32;
                                    paint_text(
                                        time_axis_label(&points, seconds),
                                        point(x - px(32.), plot.bottom() + px(8.)),
                                        px(64.),
                                        TextAlign::Center,
                                        &font,
                                        muted,
                                        window,
                                        cx,
                                    );
                                }
                                let mut ends = Vec::new();
                                for (series, label) in labels.iter().enumerate() {
                                    for segment in segments(&points, series) {
                                        let mut path = if series == 1 {
                                            PathBuilder::stroke(px(2.))
                                                .dash_array(&[px(6.), px(4.)])
                                        } else {
                                            PathBuilder::stroke(px(2.))
                                        };
                                        for (index, &(seconds, value)) in segment.iter().enumerate()
                                        {
                                            let position = scale.position(seconds, value, plot);
                                            if index == 0 {
                                                path.move_to(position);
                                            } else {
                                                path.line_to(position);
                                            }
                                        }
                                        if segment.len() > 1 {
                                            if let Ok(path) = path.build() {
                                                window.paint_path(path, colors[series]);
                                            }
                                        } else {
                                            marker(
                                                scale.position(segment[0].0, segment[0].1, plot),
                                                colors[series],
                                                surface,
                                                series == 1,
                                                window,
                                            );
                                        }
                                    }
                                    if let Some(sample) = points.last() {
                                        if let Some(value) = valid_value(sample.values[series]) {
                                            if sample.seconds.is_finite() {
                                                ends.push((
                                                    scale.position(sample.seconds, value, plot),
                                                    *label,
                                                ));
                                            }
                                        }
                                    }
                                }
                                // Direct endpoint labels only when they can remain attached and do not collide.
                                if ends.len() < 2 || (ends[0].0.y - ends[1].0.y).abs() >= px(18.) {
                                    for (position, label) in ends {
                                        paint_text(
                                            label.into(),
                                            point(position.x + px(8.), position.y - px(8.)),
                                            px(62.),
                                            TextAlign::Left,
                                            &font,
                                            muted,
                                            window,
                                            cx,
                                        );
                                    }
                                }
                                if let Some(index) = hovered {
                                    let sample = &points[index];
                                    let x = scale.position(sample.seconds, 0., plot).x;
                                    stroke_line(
                                        point(x, plot.top()),
                                        point(x, plot.bottom()),
                                        muted.opacity(0.65),
                                        1.,
                                        window,
                                    );
                                    for (series, color) in
                                        colors.iter().enumerate().take(labels.len())
                                    {
                                        if let Some(value) = valid_value(sample.values[series]) {
                                            marker(
                                                scale.position(sample.seconds, value, plot),
                                                *color,
                                                surface,
                                                series == 1,
                                                window,
                                            );
                                        }
                                    }
                                    paint_hover_readout(
                                        sample, &labels, unit, plot, bounds, x, &font, foreground,
                                        muted, surface, border, window, cx,
                                    );
                                }
                                if !points.iter().any(|sample| {
                                    sample.seconds.is_finite()
                                        && sample
                                            .values
                                            .iter()
                                            .take(labels.len())
                                            .any(|value| valid_value(*value).is_some())
                                }) {
                                    paint_text(
                                        "等待采样".into(),
                                        point(plot.left(), plot.top() + px(65.)),
                                        plot.size.width,
                                        TextAlign::Center,
                                        &font,
                                        muted,
                                        window,
                                        cx,
                                    );
                                }
                            },
                        )
                        .size_full(),
                    ),
            )
    }
}

fn plot_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    // Axis bands shrink with the card; the actual plot consumes all remaining
    // width/height instead of imposing a fixed canvas size on the layout.
    let horizontal = px(70.).min(bounds.size.width * 0.24);
    let top = px(10.).min(bounds.size.height * 0.1);
    let bottom = px(32.).min(bounds.size.height * 0.3);
    Bounds::new(
        bounds.origin + point(horizontal, top),
        size(
            (bounds.size.width - horizontal * 2.).max(px(0.)),
            (bounds.size.height - top - bottom).max(px(0.)),
        ),
    )
}

fn nearest_time(times: &[f64], seconds: f64) -> Option<f64> {
    times
        .iter()
        .copied()
        .filter(|time| time.is_finite())
        .min_by(|a, b| (a - seconds).abs().total_cmp(&(b - seconds).abs()))
}

fn hover_clock_label(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "—".into())
}

/// Keep the readout inside this chart, flipping left at the right edge.
fn readout_bounds(
    bounds: Bounds<Pixels>,
    x: Pixels,
    top: Pixels,
    width: Pixels,
    height: Pixels,
) -> Bounds<Pixels> {
    let width = width.min((bounds.size.width - px(8.)).max(px(0.)));
    let height = height.min((bounds.size.height - px(8.)).max(px(0.)));
    let inset_x = px(4.).min(bounds.size.width / 2.);
    let inset_y = px(4.).min(bounds.size.height / 2.);
    let preferred = if x + px(12.) + width <= bounds.right() - inset_x {
        x + px(12.)
    } else {
        x - px(12.) - width
    };
    let left = preferred
        .max(bounds.left() + inset_x)
        .min(bounds.right() - width - inset_x);
    let top = top
        .max(bounds.top() + inset_y)
        .min(bounds.bottom() - height - inset_y);
    Bounds::new(point(left, top), size(width, height))
}

#[allow(clippy::too_many_arguments)]
fn paint_hover_readout(
    sample: &ChartPoint,
    labels: &[&str],
    unit: ChartUnit,
    plot: Bounds<Pixels>,
    bounds: Bounds<Pixels>,
    x: Pixels,
    font: &Font,
    foreground: Hsla,
    muted: Hsla,
    surface: Hsla,
    border: Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    let rows: Vec<String> = labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            let value = format_measurement(sample.values[index], unit);
            if labels.len() == 1 {
                value
            } else {
                format!("{label}  {value}")
            }
        })
        .collect();
    let time = hover_clock_label(sample.timestamp);
    let text_width = std::iter::once(&time)
        .chain(rows.iter())
        .map(|text| {
            let run = TextRun {
                len: text.len(),
                font: font.clone(),
                color: foreground,
                ..Default::default()
            };
            window
                .text_system()
                .shape_line(text.clone().into(), px(11.), &[run], None)
                .width
        })
        .fold(px(0.), |width, next| width.max(next));
    let tooltip = readout_bounds(
        bounds,
        x,
        plot.top() + px(8.),
        text_width + px(24.),
        px(20. * (rows.len() + 1) as f32 + 16.),
    );
    if tooltip.size.width < px(26.) || tooltip.size.height < px(18.) {
        return;
    }
    // Painted after the crosshair and markers; no hitbox can steal hover events.
    window.paint_quad(fill(tooltip, border).corner_radii(px(6.)));
    window.paint_quad(
        fill(
            Bounds::new(
                tooltip.origin + point(px(1.), px(1.)),
                tooltip.size - size(px(2.), px(2.)),
            ),
            surface,
        )
        .corner_radii(px(5.)),
    );
    paint_text(
        time,
        tooltip.origin + point(px(12.), px(8.)),
        tooltip.size.width - px(24.),
        TextAlign::Left,
        font,
        muted,
        window,
        cx,
    );
    for (index, row) in rows.into_iter().enumerate() {
        let offset = px(28. + 20. * index as f32);
        if offset + px(16.) > tooltip.size.height - px(4.) {
            break;
        }
        paint_text(
            row,
            tooltip.origin + point(px(12.), offset),
            tooltip.size.width - px(24.),
            TextAlign::Left,
            font,
            foreground,
            window,
            cx,
        );
    }
}

fn line_key(color: Hsla, dashed: bool) -> impl IntoElement {
    div()
        .w(px(18.))
        .h(px(2.))
        .flex_shrink_0()
        .flex()
        .gap(px(3.))
        .children(
            (0..if dashed { 3 } else { 1 })
                .map(|_| div().flex_1().h_full().rounded_full().bg(color)),
        )
}

#[derive(Clone, Copy)]
struct Scale {
    start: f64,
    end: f64,
    ceiling: f64,
}

impl Scale {
    fn new(points: &[ChartPoint], count: usize, unit: ChartUnit) -> Self {
        let mut start = f64::INFINITY;
        let mut end = f64::NEG_INFINITY;
        let mut maximum: f64 = 0.;
        for sample in points.iter().filter(|sample| sample.seconds.is_finite()) {
            start = start.min(sample.seconds);
            end = end.max(sample.seconds);
            for value in sample
                .values
                .iter()
                .take(count)
                .filter_map(|value| valid_value(*value))
            {
                maximum = maximum.max(value);
            }
        }
        if !start.is_finite() {
            start = 0.;
            end = 1.;
        }
        if end <= start {
            start = (start - 1.).max(0.);
            end += 1.;
        }
        let minimum = match unit {
            ChartUnit::Percent => 1.,
            ChartUnit::Bytes => 1024.,
        };
        let target = (maximum * 1.15).max(minimum) / 4.;
        let power = 10_f64.powf(target.log10().floor());
        let normalized = target / power;
        let step = [1., 2., 2.5, 5., 10.]
            .into_iter()
            .find(|step| *step >= normalized)
            .unwrap_or(10.);
        Self {
            start,
            end,
            ceiling: step * power * 4.,
        }
    }

    fn position(self, seconds: f64, value: f64, plot: Bounds<Pixels>) -> Point<Pixels> {
        point(
            plot.left()
                + plot.size.width * ((seconds - self.start) / (self.end - self.start)) as f32,
            plot.bottom() - plot.size.height * (value / self.ceiling) as f32,
        )
    }
}

fn valid_value(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && *value >= 0.)
}

/// Keep gaps local to a series, and never bridge a reconnect or a backwards timestamp.
fn segments(points: &[ChartPoint], series: usize) -> Vec<Vec<(f64, f64)>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    for sample in points {
        let value = valid_value(sample.values[series]).filter(|_| sample.seconds.is_finite());
        let backwards = current
            .last()
            .is_some_and(|&(seconds, _)| sample.seconds < seconds);
        if (!sample.connected || value.is_none() || backwards) && !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
        if let Some(value) = value {
            current.push((sample.seconds, value));
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

fn nearest_point(points: &[ChartPoint], seconds: f64) -> Option<usize> {
    points
        .iter()
        .enumerate()
        .filter(|(_, sample)| sample.seconds.is_finite())
        .min_by(|(_, a), (_, b)| {
            (a.seconds - seconds)
                .abs()
                .total_cmp(&(b.seconds - seconds).abs())
        })
        .map(|(index, _)| index)
}

fn clock_label(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.format("%H:%M").to_string())
        .unwrap_or_else(|| "—".into())
}

fn time_axis_label(points: &[ChartPoint], seconds: f64) -> String {
    let Some(index) = nearest_point(points, seconds) else {
        return "—".into();
    };
    let sample = &points[index];
    clock_label(
        sample
            .timestamp
            .saturating_add((seconds - sample.seconds).round() as i64),
    )
}

fn axis_label(value: f64, ceiling: f64, unit: ChartUnit) -> String {
    match unit {
        ChartUnit::Percent => format!("{}%", compact_decimal(value)),
        ChartUnit::Bytes => {
            let (divisor, suffix) = byte_unit(ceiling);
            format!("{} {suffix}", compact_decimal(value / divisor))
        }
    }
}

fn format_measurement(value: Option<f64>, unit: ChartUnit) -> String {
    let Some(value) = valid_value(value) else {
        return "—".into();
    };
    match unit {
        ChartUnit::Percent => format!("{value:.2}%"),
        ChartUnit::Bytes => {
            let (divisor, suffix) = byte_unit(value);
            format!("{} {suffix}", compact_decimal(value / divisor))
        }
    }
}

fn byte_unit(value: f64) -> (f64, &'static str) {
    for (power, suffix) in [(5, "PiB"), (4, "TiB"), (3, "GiB"), (2, "MiB"), (1, "KiB")] {
        let divisor = 1024_f64.powi(power);
        if value >= divisor {
            return (divisor, suffix);
        }
    }
    (1., "B")
}

fn compact_decimal(value: f64) -> String {
    if value == 0. || value >= 100. || value.fract().abs() < 0.001 {
        format!("{value:.0}")
    } else if value >= 1. {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn stroke_line(
    from: Point<Pixels>,
    to: Point<Pixels>,
    color: Hsla,
    width: f32,
    window: &mut Window,
) {
    let mut path = PathBuilder::stroke(px(width));
    path.move_to(from);
    path.line_to(to);
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn marker(
    position: Point<Pixels>,
    color: Hsla,
    surface: Hsla,
    secondary: bool,
    window: &mut Window,
) {
    if secondary {
        // A hollow diamond preserves the first series' dot even for equal values.
        let mut path = PathBuilder::stroke(px(2.));
        path.move_to(position - point(px(0.), px(6.)));
        path.line_to(position + point(px(6.), px(0.)));
        path.line_to(position + point(px(0.), px(6.)));
        path.line_to(position - point(px(6.), px(0.)));
        path.close();
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
        return;
    }
    window.paint_quad(
        fill(
            Bounds::new(position - point(px(6.), px(6.)), size(px(12.), px(12.))),
            surface,
        )
        .corner_radii(px(6.)),
    );
    window.paint_quad(
        fill(
            Bounds::new(position - point(px(4.), px(4.)), size(px(8.), px(8.))),
            color,
        )
        .corner_radii(px(4.)),
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_text(
    text: String,
    position: Point<Pixels>,
    width: Pixels,
    align: TextAlign,
    font: &Font,
    color: Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    let run = TextRun {
        len: text.len(),
        font: font.clone(),
        color,
        ..Default::default()
    };
    let line = window
        .text_system()
        .shape_line(text.into(), px(11.), &[run], None);
    // Endpoint labels are optional; do not let a long caller-supplied name escape the card.
    if line.width > width {
        return;
    }
    let offset = match align {
        TextAlign::Right => width - line.width,
        TextAlign::Center => (width - line.width) / 2.,
        _ => px(0.),
    };
    _ = line.paint(
        position + point(offset, px(0.)),
        px(16.),
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(seconds: f64, a: Option<f64>, b: Option<f64>, connected: bool) -> ChartPoint {
        ChartPoint {
            seconds,
            timestamp: seconds as i64,
            values: [a, b],
            connected,
        }
    }

    #[test]
    fn gaps_break_only_the_affected_series_and_reconnects_break_both() {
        let points = vec![
            sample(0., Some(1.), Some(2.), false),
            sample(2., None, Some(3.), true),
            sample(8., Some(4.), Some(5.), true),
            sample(9., Some(6.), Some(7.), false),
        ];
        assert_eq!(
            segments(&points, 0),
            vec![vec![(0., 1.)], vec![(8., 4.)], vec![(9., 6.)]]
        );
        assert_eq!(
            segments(&points, 1),
            vec![vec![(0., 2.), (2., 3.), (8., 5.)], vec![(9., 7.)]]
        );
    }

    #[test]
    fn cpu_scale_has_headroom_above_one_hundred_and_zero_baseline() {
        let scale = Scale::new(
            &[sample(0., Some(245.), None, false)],
            1,
            ChartUnit::Percent,
        );
        assert!(scale.ceiling > 245.);
        let plot = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.)));
        assert_eq!(scale.position(0., 0., plot).y, plot.bottom());
        assert!(scale.end > scale.start);
    }

    #[test]
    fn x_positions_and_hover_use_seconds_not_sample_indices() {
        let points = vec![
            sample(0., Some(0.), None, false),
            sample(1., Some(1.), None, true),
            sample(10., Some(2.), None, true),
        ];
        let scale = Scale::new(&points, 1, ChartUnit::Percent);
        let plot = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.)));
        assert_eq!(scale.position(1., 0., plot).x, px(10.));
        assert_eq!(nearest_point(&points, 6.), Some(2));
    }

    #[test]
    fn plot_geometry_fills_available_space_without_overflow() {
        for (width, height) in [
            (0., 0.),
            (100., 40.),
            (268., 120.),
            (600., 350.),
            (1000., 700.),
        ] {
            let bounds = Bounds::new(point(px(100.), px(50.)), size(px(width), px(height)));
            let plot = plot_bounds(bounds);
            assert!(plot.size.width >= px(0.));
            assert!(plot.size.height >= px(0.));
            assert!(plot.left() >= bounds.left());
            assert!(plot.top() >= bounds.top());
            assert!(plot.right() <= bounds.right());
            assert!(plot.bottom() <= bounds.bottom());
        }
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(600.), px(350.)));
        let plot = plot_bounds(bounds);
        assert_eq!(plot.size.width, px(460.));
        assert_eq!(plot.size.height, px(308.));
    }

    #[test]
    fn readout_stays_within_short_and_narrow_charts() {
        for (width, height) in [(0., 0.), (6., 6.), (100., 40.), (268., 120.)] {
            let bounds = Bounds::new(point(px(100.), px(50.)), size(px(width), px(height)));
            let tooltip = readout_bounds(
                bounds,
                bounds.right(),
                bounds.top() + px(18.),
                px(160.),
                px(76.),
            );
            assert!(tooltip.left() >= bounds.left());
            assert!(tooltip.right() <= bounds.right());
            assert!(tooltip.top() >= bounds.top());
            assert!(tooltip.bottom() <= bounds.bottom());
        }
    }

    #[test]
    fn shared_hover_selects_the_same_sample_in_all_four_charts() {
        let selected = nearest_time(&[0., 10., 23., 30.], 21.).unwrap();
        assert_eq!(selected, 23.);
        for series in 0..4 {
            let mut points = vec![
                sample(0., Some(series as f64), None, false),
                sample(10., None, None, true),
                sample(23., Some(series as f64 + 2.), None, true),
                sample(30., Some(series as f64 + 3.), None, true),
            ];
            assert_eq!(nearest_point(&points, selected), Some(2));
            points.push(sample(40., Some(4.), None, true));
            assert_eq!(nearest_point(&points, selected), Some(2));
            assert_eq!(nearest_point(&points, 10.), Some(1)); // Missing values still have a time.
        }
        assert_eq!(nearest_time(&[], 0.), None);
        assert_eq!(nearest_time(&[f64::NAN, 10.], 0.), Some(10.));
    }

    #[test]
    fn readout_stays_inside_each_chart_and_flips_at_the_right_edge() {
        for width in [268., 500., 900.] {
            let bounds = Bounds::new(point(px(100.), px(50.)), size(px(width), px(222.)));
            for x in [
                bounds.left(),
                bounds.left() + px(width / 2.),
                bounds.right(),
            ] {
                let tooltip = readout_bounds(bounds, x, px(68.), px(160.), px(76.));
                assert!(tooltip.left() >= bounds.left());
                assert!(tooltip.right() <= bounds.right());
                assert!(tooltip.top() >= bounds.top());
                assert!(tooltip.bottom() <= bounds.bottom());
                if x == bounds.right() {
                    assert!(tooltip.right() < x);
                }
            }
        }
    }

    #[test]
    fn hover_time_includes_seconds_and_clear_resets_the_shared_selection() {
        let time = Local.with_ymd_and_hms(2026, 11, 8, 9, 7, 23).unwrap();
        assert_eq!(hover_clock_label(time.timestamp()), "09:07:23");
        let mut hover = StatsHover {
            selected: Some(23.),
            source: Some("cpu"),
        };
        hover.clear();
        assert!(hover.selected.is_none());
        assert!(hover.source.is_none());
    }

    #[test]
    fn time_labels_use_local_twenty_four_hour_clock_and_wrap_midnight() {
        for (hour, minute, expected) in [(0, 5, "00:05"), (9, 7, "09:07"), (23, 59, "23:59")] {
            let time = Local
                .with_ymd_and_hms(2026, 11, 8, hour, minute, 0)
                .unwrap();
            assert_eq!(clock_label(time.timestamp()), expected);
        }
        let time = Local.with_ymd_and_hms(2026, 11, 8, 23, 59, 0).unwrap();
        let mut point = sample(0., Some(1.), None, false);
        point.timestamp = time.timestamp();
        assert_eq!(time_axis_label(&[point], 120.), "00:01");
        assert_eq!(time_axis_label(&[], 0.), "—");
    }

    #[test]
    fn empty_zero_and_invalid_data_remain_finite_and_missing() {
        for points in [
            vec![],
            vec![sample(0., Some(0.), None, false)],
            vec![sample(f64::NAN, Some(f64::INFINITY), None, true)],
        ] {
            let scale = Scale::new(&points, 1, ChartUnit::Bytes);
            assert!(scale.ceiling.is_finite() && scale.ceiling > 0.);
            assert!(scale.start.is_finite() && scale.end > scale.start);
        }
        assert_eq!(format_measurement(None, ChartUnit::Bytes), "—");
        assert_eq!(format_measurement(Some(-1.), ChartUnit::Percent), "—");
        assert_eq!(format_measurement(Some(1024.), ChartUnit::Bytes), "1 KiB");
    }
}
