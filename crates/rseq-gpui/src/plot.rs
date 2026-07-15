use gpui::{App, Bounds, Hsla, PathBuilder, Pixels, Window, fill, point, px};
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
pub struct EventMarker {
    pub x: f32,
    pub color: Hsla,
}

#[derive(Clone, Copy, Debug)]
struct ProjectedPoint {
    x: f32,
    y: f32,
    depth: f32,
}

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
    event_markers: Vec<EventMarker>,
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
        Self::new_with_ranges_axes_and_markers(
            data,
            colors,
            visible_axes,
            x_min,
            x_max,
            y_min,
            y_max,
            Vec::new(),
        )
    }

    pub fn new_with_ranges_axes_and_markers(
        data: Vec<Vec3>,
        colors: [Hsla; 3],
        visible_axes: [bool; 3],
        x_min: f32,
        x_max: f32,
        y_min: f32,
        y_max: f32,
        event_markers: Vec<EventMarker>,
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
            event_markers,
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

        for marker in &self.event_markers {
            if marker.x < self.x_min || marker.x > self.x_max {
                continue;
            }
            let Some(x_pos) = x.tick(&(marker.x as f64)) else {
                continue;
            };
            let mut line = PathBuilder::stroke(px(1.));
            line.move_to(origin_point(px(x_pos), px(0.), bounds.origin));
            line.line_to(origin_point(px(x_pos), px(height), bounds.origin));
            if let Ok(path) = line.build() {
                window.paint_path(path, marker.color.opacity(0.30));
            }
            let mark = Bounds::from_corners(
                origin_point(px((x_pos - 3.).max(0.)), px(2.), bounds.origin),
                origin_point(px((x_pos + 3.).min(width)), px(8.), bounds.origin),
            );
            window.paint_quad(fill(mark, marker.color.opacity(0.88)));
        }

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

#[derive(IntoPlot)]
pub struct OrientationModel {
    roll: f32,
    pitch: f32,
    yaw: f32,
    face_colors: [Hsla; 6],
    axis_colors: [Hsla; 3],
    edge_color: Hsla,
    muted_color: Hsla,
}

impl OrientationModel {
    pub fn new(
        roll: f32,
        pitch: f32,
        yaw: f32,
        face_colors: [Hsla; 6],
        axis_colors: [Hsla; 3],
        edge_color: Hsla,
        muted_color: Hsla,
    ) -> Self {
        Self {
            roll,
            pitch,
            yaw,
            face_colors,
            axis_colors,
            edge_color,
            muted_color,
        }
    }

    fn rotate(&self, [x, y, z]: Vec3) -> Vec3 {
        let (sr, cr) = self.roll.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        let (sy, cy) = self.yaw.sin_cos();

        let roll = [x, y * cr - z * sr, y * sr + z * cr];
        let pitch = [
            roll[0] * cp + roll[2] * sp,
            roll[1],
            -roll[0] * sp + roll[2] * cp,
        ];
        [
            pitch[0] * cy - pitch[1] * sy,
            pitch[0] * sy + pitch[1] * cy,
            pitch[2],
        ]
    }

    fn project(point_3d: Vec3, bounds: Bounds<Pixels>, scale: f32) -> ProjectedPoint {
        let origin = bounds.origin;
        let width = bounds.size.width.as_f32();
        let height = bounds.size.height.as_f32();
        let x = point_3d[0] - point_3d[1] * 0.42;
        let y = -point_3d[2] + point_3d[1] * 0.32;

        ProjectedPoint {
            x: origin.x.as_f32() + width * 0.5 + x * scale,
            y: origin.y.as_f32() + height * 0.52 + y * scale,
            depth: point_3d[1] + point_3d[2] * 0.12,
        }
    }

    fn rotated_projected(
        &self,
        point_3d: Vec3,
        bounds: Bounds<Pixels>,
        scale: f32,
    ) -> ProjectedPoint {
        Self::project(self.rotate(point_3d), bounds, scale)
    }
}

impl Plot for OrientationModel {
    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, _cx: &mut App) {
        let width = bounds.size.width.as_f32();
        let height = bounds.size.height.as_f32();
        if width <= 4.0 || height <= 4.0 {
            return;
        }

        let scale = width.min(height) * 0.34;
        let vertices = [
            [-0.55, -0.55, -0.55],
            [0.55, -0.55, -0.55],
            [0.55, 0.55, -0.55],
            [-0.55, 0.55, -0.55],
            [-0.55, -0.55, 0.55],
            [0.55, -0.55, 0.55],
            [0.55, 0.55, 0.55],
            [-0.55, 0.55, 0.55],
        ];
        let projected = vertices.map(|vertex| self.rotated_projected(vertex, bounds, scale));
        let faces = [
            ([0, 1, 2, 3], 0),
            ([4, 7, 6, 5], 1),
            ([0, 4, 5, 1], 2),
            ([3, 2, 6, 7], 3),
            ([0, 3, 7, 4], 4),
            ([1, 5, 6, 2], 5),
        ];
        let mut ordered_faces = faces
            .into_iter()
            .map(|(indices, color_index)| {
                let depth = indices
                    .iter()
                    .map(|index| projected[*index].depth)
                    .sum::<f32>()
                    / indices.len() as f32;
                (depth, indices, color_index)
            })
            .collect::<Vec<_>>();
        ordered_faces.sort_by(|a, b| b.0.total_cmp(&a.0));

        let shadow_bounds = Bounds::from_corners(
            point(
                px(bounds.origin.x.as_f32() + width * 0.23),
                px(bounds.origin.y.as_f32() + height * 0.70),
            ),
            point(
                px(bounds.origin.x.as_f32() + width * 0.77),
                px(bounds.origin.y.as_f32() + height * 0.79),
            ),
        );
        window.paint_quad(fill(shadow_bounds, self.muted_color.opacity(0.16)));

        for (_, indices, color_index) in ordered_faces {
            let mut fill_builder = PathBuilder::fill();
            let first = projected[indices[0]];
            fill_builder.move_to(point(px(first.x), px(first.y)));
            for index in indices.iter().skip(1) {
                let point_2d = projected[*index];
                fill_builder.line_to(point(px(point_2d.x), px(point_2d.y)));
            }
            if let Ok(path) = fill_builder.build() {
                window.paint_path(path, self.face_colors[color_index]);
            }

            let mut edge_builder = PathBuilder::stroke(px(1.0));
            edge_builder.move_to(point(px(first.x), px(first.y)));
            for index in indices.iter().skip(1) {
                let point_2d = projected[*index];
                edge_builder.line_to(point(px(point_2d.x), px(point_2d.y)));
            }
            edge_builder.line_to(point(px(first.x), px(first.y)));
            if let Ok(path) = edge_builder.build() {
                window.paint_path(path, self.edge_color.opacity(0.72));
            }
        }

        let axis_ends = [
            self.rotated_projected([0.95, 0.0, 0.0], bounds, scale),
            self.rotated_projected([0.0, 0.95, 0.0], bounds, scale),
            self.rotated_projected([0.0, 0.0, 0.95], bounds, scale),
        ];
        let origin = self.rotated_projected([0.0, 0.0, 0.0], bounds, scale);
        for (axis, end) in axis_ends.into_iter().enumerate() {
            let mut axis_builder = PathBuilder::stroke(px(2.0));
            axis_builder.move_to(point(px(origin.x), px(origin.y)));
            axis_builder.line_to(point(px(end.x), px(end.y)));
            if let Ok(path) = axis_builder.build() {
                window.paint_path(path, self.axis_colors[axis].opacity(0.86));
            }
        }
    }
}
