//! 迷你图：分时面积图与日 K 蜡烛图，去掉坐标轴和悬浮层，只看形态。

use gpui_kit::component::chart::{AreaChart, CandlestickChart};
use gpui_kit::component::{v_flex, ActiveTheme as _};
use gpui_kit::{
    div, linear_color_stop, linear_gradient, prelude::FluentBuilder as _, px, App, ElementId, Hsla,
    IntoElement, ParentElement as _, Pixels, RenderOnce, SharedString, Styled as _, Window,
};

use crate::feed::Candle;

/// 分时曲线上的一个绘图点。
#[derive(Clone, Debug)]
pub struct ChartPoint {
    pub label: SharedString,
    pub value: f64,
}

/// 当前展示的序列。图卡与顶部大字报价共用同一份数据。
#[derive(Clone, Debug)]
pub enum Series {
    Intraday(Vec<ChartPoint>),
    Daily(Vec<Candle>),
}

impl Series {
    /// 序列里的最高 / 最低价，用于在图表上方标注区间。
    pub fn extent(&self) -> Option<(f64, f64)> {
        let values: Vec<f64> = match self {
            Series::Intraday(points) => points.iter().map(|p| p.value).collect(),
            Series::Daily(candles) => candles.iter().flat_map(|c| [c.high, c.low]).collect(),
        };
        let hi = values.iter().copied().fold(f64::MIN, f64::max);
        let lo = values.iter().copied().fold(f64::MAX, f64::min);
        (!values.is_empty() && hi > lo).then_some((lo, hi))
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Series::Intraday(points) => points.is_empty(),
            Series::Daily(candles) => candles.is_empty(),
        }
    }
}

/// 一张迷你图。
///
/// `PlotElement` 自己按 100% 撑满，必须由父容器给定量高度，所以高度是必填项。
#[derive(IntoElement)]
pub struct MiniChart {
    id: ElementId,
    series: Series,
    height: Pixels,
    empty_text: SharedString,
}

impl MiniChart {
    pub fn new(id: impl Into<ElementId>, series: Series) -> Self {
        Self {
            id: id.into(),
            series,
            height: px(130.),
            empty_text: "暂无数据".into(),
        }
    }

    /// 曲线区域高度。
    pub fn height(mut self, height: Pixels) -> Self {
        self.height = height;
        self
    }

    /// 没有数据时显示的提示，比如"休市中"或"正在获取行情…"。
    pub fn empty_text(mut self, text: impl Into<SharedString>) -> Self {
        self.empty_text = text.into();
        self
    }
}

impl RenderOnce for MiniChart {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let accent = theme.chart_1;
        let bullish = theme.chart_bullish;
        let bearish = theme.chart_bearish;
        let muted = theme.muted_foreground;
        let empty_text = self.empty_text;

        let chart = match self.series {
            Series::Intraday(points) if !points.is_empty() => {
                let domain = price_domain(points.iter().map(|p| p.value));
                let fill = linear_gradient(
                    180.,
                    linear_color_stop(accent.opacity(0.35), 0.0),
                    linear_color_stop(accent.opacity(0.02), 1.0),
                );
                AreaChart::new(points)
                    .id(self.id)
                    .x(|point: &ChartPoint| point.label.clone())
                    .y(|point: &ChartPoint| point.value)
                    .stroke(accent)
                    .fill(fill)
                    .linear()
                    // 图卡很小，坐标轴和悬浮层只留最必要的，避免糊成一团。
                    .interactive(false)
                    .grid(false)
                    .x_axis(false)
                    .y_axis(true)
                    .y_tick_count(2)
                    .y_tick_format(fmt_price)
                    .y_padding(10., 10.)
                    .when_some(domain, |chart, (lo, hi)| chart.y_domain(lo, hi))
                    .into_any_element()
            }
            Series::Daily(candles) if !candles.is_empty() => {
                let tick_margin = (candles.len() / 2).max(1);
                CandlestickChart::new(candles)
                    .id(self.id)
                    .x(|candle: &Candle| candle.date.clone())
                    .open(|candle: &Candle| candle.open)
                    .high(|candle: &Candle| candle.high)
                    .low(|candle: &Candle| candle.low)
                    .close(|candle: &Candle| candle.close)
                    .bullish(bullish)
                    .bearish(bearish)
                    .interactive(false)
                    .grid(false)
                    .x_axis(false)
                    .tick_margin(tick_margin)
                    .into_any_element()
            }
            _ => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(muted)
                .child(empty_text)
                .into_any_element(),
        };

        div().w_full().h(self.height).child(chart)
    }
}

/// 价格按量级选小数位：比特币、伦锡上万美元，天然气只有 3 美元。
pub fn fmt_price(value: f64) -> String {
    if !value.is_finite() {
        return "--".to_string();
    }
    let abs = value.abs();
    if abs >= 100.0 {
        format!("{value:.1}")
    } else if abs >= 10.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.3}")
    }
}

/// 给折线留一点上下留白；数据无波动时返回 `None`（交给图表自己处理）。
fn price_domain(values: impl Iterator<Item = f64>) -> Option<(f64, f64)> {
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for value in values {
        if !value.is_finite() {
            continue;
        }
        lo = lo.min(value);
        hi = hi.max(value);
    }
    if !(hi > lo) {
        return None;
    }
    let padding = (hi - lo) * 0.08;
    Some((lo - padding, hi + padding))
}

/// 涨跌配色：国内口径，红涨绿跌。
pub fn change_color(change: Option<f64>, cx: &App) -> Hsla {
    let theme = cx.theme();
    match change {
        Some(c) if c > 0.0 => theme.chart_bullish,
        Some(c) if c < 0.0 => theme.chart_bearish,
        _ => theme.foreground,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_adds_eight_percent_padding() {
        let (lo, hi) = price_domain([100.0, 200.0].into_iter()).expect("domain");
        assert!((lo - 92.0).abs() < 1e-9);
        assert!((hi - 208.0).abs() < 1e-9);
    }

    #[test]
    fn flat_series_has_no_domain() {
        assert!(price_domain([100.0, 100.0].into_iter()).is_none());
        assert!(price_domain(std::iter::empty()).is_none());
    }

    #[test]
    fn price_precision_follows_magnitude() {
        assert_eq!(fmt_price(81781.95), "81781.9");
        assert_eq!(fmt_price(53.888), "53.89");
        assert_eq!(fmt_price(3.485), "3.485");
        assert_eq!(fmt_price(f64::NAN), "--");
    }
}
