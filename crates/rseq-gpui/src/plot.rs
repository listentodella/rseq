use gpui::{App, Bounds, Hsla, PathBuilder, Pixels, Window, fill, px};
use gpui_component::{
    ActiveTheme,
    plot::{
        Grid, IntoPlot, Plot, origin_point,
        scale::{Scale, ScaleLinear},
        shape::Line,
    },
};

pub type Vec3 = [f32; 3];

#[derive(Clone, Copy, Debug)]
pub struct AxisOhlc {
    pub open: f32,
    pub high: f32,
    pub low: f32,
    pub close: f32,
}

impl AxisOhlc {
    pub fn new(value: f32) -> Self {
        Self {
            open: value,
            high: value,
            low: value,
            close: value,
        }
    }

    pub fn push(&mut self, value: f32) {
        self.high = self.high.max(value);
        self.low = self.low.min(value);
        self.close = value;
    }

    fn peak_abs(&self) -> f32 {
        self.open
            .abs()
            .max(self.high.abs())
            .max(self.low.abs())
            .max(self.close.abs())
    }
}

pub type TripleOhlc = [AxisOhlc; 3];

pub fn new_triple_ohlc(sample: Vec3) -> TripleOhlc {
    [
        AxisOhlc::new(sample[0]),
        AxisOhlc::new(sample[1]),
        AxisOhlc::new(sample[2]),
    ]
}

pub fn push_triple_ohlc(bucket: &mut TripleOhlc, sample: Vec3) {
    for axis in 0..3 {
        bucket[axis].push(sample[axis]);
    }
}

#[derive(IntoPlot)]
pub struct ScalarLineChart {
    data: Vec<f32>,
    color: Hsla,
    y_min: f32,
    y_max: f32,
}

impl ScalarLineChart {
    pub fn new(data: Vec<f32>, color: Hsla, min_span: f32) -> Self {
        let (mut y_min, mut y_max) = data
            .first()
            .map(|first| {
                data.iter().fold((*first, *first), |(min, max), value| {
                    (min.min(*value), max.max(*value))
                })
            })
            .unwrap_or((0.0, min_span));
        let span = (y_max - y_min).max(min_span);
        if (y_max - y_min) < span {
            let center = (y_min + y_max) / 2.0;
            y_min = center - span / 2.0;
            y_max = center + span / 2.0;
        }
        let padding = span * 0.08;
        Self {
            data,
            color,
            y_min: y_min - padding,
            y_max: y_max + padding,
        }
    }
}

impl Plot for ScalarLineChart {
    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let n = self.data.len();
        if n < 2 {
            return;
        }

        let width = bounds.size.width.as_f32();
        let height = bounds.size.height.as_f32();
        let x = ScaleLinear::new(vec![0.0f64, (n - 1) as f64], vec![0., width]);
        let y = ScaleLinear::new(vec![self.y_min as f64, self.y_max as f64], vec![height, 0.]);

        Grid::new()
            .y((0..=4).map(|i| height * i as f32 / 4.0).collect())
            .stroke(cx.theme().border)
            .dash_array(&[px(4.), px(2.)])
            .paint(&bounds, window);

        let series = self
            .data
            .iter()
            .enumerate()
            .map(|(index, value)| (index, *value))
            .collect::<Vec<_>>();

        Line::new()
            .data(series)
            .x(move |point: &(usize, f32)| x.tick(&(point.0 as f64)))
            .y(move |point: &(usize, f32)| y.tick(&(point.1 as f64)))
            .stroke(self.color)
            .stroke_width(1.6)
            .paint(&bounds, window);
    }
}

#[derive(IntoPlot)]
pub struct TripleLineChart {
    data: Vec<Vec3>,
    colors: [Hsla; 3],
    visible_axes: [bool; 3],
    x_min: f32,
    x_max: f32,
    y_min: f32,
    y_max: f32,
}

impl TripleLineChart {
    pub fn new_with_ranges_and_axes(
        data: Vec<Vec3>,
        colors: [Hsla; 3],
        visible_axes: [bool; 3],
        x_min: f32,
        x_max: f32,
        y_min: f32,
        y_max: f32,
    ) -> Self {
        let (x_min, x_max) = if x_max > x_min {
            (x_min, x_max)
        } else {
            (0.0, 1.0)
        };
        let (y_min, y_max) = if y_max > y_min {
            (y_min, y_max)
        } else {
            (-1.0, 1.0)
        };
        Self {
            data,
            colors,
            visible_axes,
            x_min,
            x_max,
            y_min,
            y_max,
        }
    }
}

impl Plot for TripleLineChart {
    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let n = self.data.len();
        if n < 2 {
            return;
        }

        let width = bounds.size.width.as_f32();
        let height = bounds.size.height.as_f32();
        let x = ScaleLinear::new(vec![self.x_min as f64, self.x_max as f64], vec![0., width]);
        let y = ScaleLinear::new(vec![self.y_min as f64, self.y_max as f64], vec![height, 0.]);

        Grid::new()
            .y((0..=4).map(|i| height * i as f32 / 4.0).collect())
            .stroke(cx.theme().border)
            .dash_array(&[px(4.), px(2.)])
            .paint(&bounds, window);

        for axis in 0..3 {
            if !self.visible_axes[axis] {
                continue;
            }

            let xs = x.clone();
            let ys = y.clone();
            let series = self
                .data
                .iter()
                .enumerate()
                .filter(|(idx, _)| {
                    let idx = *idx as f32;
                    idx >= self.x_min && idx <= self.x_max
                })
                .map(|(idx, sample)| (idx, sample[axis]))
                .collect::<Vec<_>>();

            Line::new()
                .data(series)
                .x(move |point: &(usize, f32)| xs.tick(&(point.0 as f64)))
                .y(move |point: &(usize, f32)| ys.tick(&(point.1 as f64)))
                .stroke(self.colors[axis])
                .stroke_width(1.5)
                .paint(&bounds, window);
        }

        let zero_y = y.tick(&0.0f64).unwrap_or(height / 2.0);
        let mut zero = PathBuilder::stroke(px(1.));
        zero.move_to(origin_point(px(0.), px(zero_y), bounds.origin));
        zero.line_to(origin_point(px(width), px(zero_y), bounds.origin));
        if let Ok(path) = zero.build() {
            window.paint_path(path, cx.theme().muted_foreground.opacity(0.42));
        }
    }
}

#[derive(IntoPlot)]
pub struct TripleOhlcChart {
    data: Vec<TripleOhlc>,
    colors: [Hsla; 3],
    visible_axes: [bool; 3],
    y_abs: f32,
}

impl TripleOhlcChart {
    pub fn new_with_axes(
        data: Vec<TripleOhlc>,
        colors: [Hsla; 3],
        visible_axes: [bool; 3],
        min_span: f32,
    ) -> Self {
        let peak = data
            .iter()
            .flat_map(|bucket| {
                bucket
                    .iter()
                    .enumerate()
                    .filter_map(|(axis, ohlc)| visible_axes[axis].then_some(ohlc))
            })
            .fold(0f32, |max, ohlc| max.max(ohlc.peak_abs()));
        let y_abs = (peak * 1.15).max(min_span);
        Self {
            data,
            colors,
            visible_axes,
            y_abs,
        }
    }
}

impl Plot for TripleOhlcChart {
    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let n = self.data.len();
        if n == 0 {
            return;
        }

        let width = bounds.size.width.as_f32();
        let height = bounds.size.height.as_f32();
        let y = ScaleLinear::new(
            vec![-(self.y_abs as f64), self.y_abs as f64],
            vec![height, 0.],
        );

        Grid::new()
            .y((0..=4).map(|i| height * i as f32 / 4.0).collect())
            .stroke(cx.theme().border)
            .dash_array(&[px(4.), px(2.)])
            .paint(&bounds, window);

        let zero_y = y.tick(&0.0f64).unwrap_or(height / 2.0);
        let mut zero = PathBuilder::stroke(px(1.));
        zero.move_to(origin_point(px(0.), px(zero_y), bounds.origin));
        zero.line_to(origin_point(px(width), px(zero_y), bounds.origin));
        if let Ok(path) = zero.build() {
            window.paint_path(path, cx.theme().muted_foreground.opacity(0.42));
        }

        let band_width = width / n as f32;
        let group_width = (band_width * 0.78).clamp(2.4, 36.);
        let axis_gap = group_width / 3.;
        let body_width = (axis_gap * 0.48).clamp(0.75, 7.);
        let origin = bounds.origin;

        for (index, bucket) in self.data.iter().enumerate() {
            let group_center = (index as f32 + 0.5) * band_width;

            for axis in 0..3 {
                if !self.visible_axes[axis] {
                    continue;
                }

                let ohlc = bucket[axis];
                let Some(open_y) = y.tick(&(ohlc.open as f64)) else {
                    continue;
                };
                let Some(high_y) = y.tick(&(ohlc.high as f64)) else {
                    continue;
                };
                let Some(low_y) = y.tick(&(ohlc.low as f64)) else {
                    continue;
                };
                let Some(close_y) = y.tick(&(ohlc.close as f64)) else {
                    continue;
                };

                let axis_center = group_center + (axis as f32 - 1.) * axis_gap;
                let color = if ohlc.close >= ohlc.open {
                    self.colors[axis]
                } else {
                    self.colors[axis].opacity(0.46)
                };

                let mut wick = PathBuilder::stroke(px(1.));
                wick.move_to(origin_point(px(axis_center), px(high_y), origin));
                wick.line_to(origin_point(px(axis_center), px(low_y), origin));
                if let Ok(path) = wick.build() {
                    window.paint_path(path, color);
                }

                let mut top = open_y.min(close_y);
                let mut bottom = open_y.max(close_y);
                if bottom - top < 1.5 {
                    let mid = (top + bottom) / 2.;
                    top = (mid - 0.75).max(0.);
                    bottom = (mid + 0.75).min(height);
                }

                let body_bounds = Bounds::from_corners(
                    origin_point(px(axis_center - body_width / 2.), px(top), origin),
                    origin_point(px(axis_center + body_width / 2.), px(bottom), origin),
                );
                window.paint_quad(fill(body_bounds, color));
            }
        }
    }
}
