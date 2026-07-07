use gpui::{App, Bounds, Hsla, Pixels, Window, px};
use gpui_component::{
    ActiveTheme,
    plot::{
        Grid, IntoPlot, Plot, origin_point,
        scale::{Scale, ScaleLinear},
        shape::Line,
    },
};

pub type Vec3 = [f32; 3];

#[derive(IntoPlot)]
pub struct TripleLineChart {
    data: Vec<Vec3>,
    colors: [Hsla; 3],
    y_abs: f32,
}

impl TripleLineChart {
    pub fn new(data: Vec<Vec3>, colors: [Hsla; 3], min_span: f32) -> Self {
        let peak = data
            .iter()
            .flat_map(|sample| sample.iter())
            .fold(0f32, |max, value| max.max(value.abs()));
        let y_abs = (peak * 1.15).max(min_span);
        Self {
            data,
            colors,
            y_abs,
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
        let x = ScaleLinear::new(vec![0.0f64, (n - 1) as f64], vec![0., width]);
        let y = ScaleLinear::new(
            vec![-(self.y_abs as f64), self.y_abs as f64],
            vec![height, 0.],
        );

        Grid::new()
            .y((0..=4).map(|i| height * i as f32 / 4.0).collect())
            .stroke(cx.theme().border)
            .dash_array(&[px(4.), px(2.)])
            .paint(&bounds, window);

        for axis in 0..3 {
            let xs = x.clone();
            let ys = y.clone();
            let series = self
                .data
                .iter()
                .enumerate()
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
        let mut zero = gpui::PathBuilder::stroke(px(1.));
        zero.move_to(origin_point(px(0.), px(zero_y), bounds.origin));
        zero.line_to(origin_point(px(width), px(zero_y), bounds.origin));
        if let Ok(path) = zero.build() {
            window.paint_path(path, cx.theme().muted_foreground.opacity(0.42));
        }
    }
}
