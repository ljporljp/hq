//! 顶部大字报价：名称、最新价、涨跌，以及今开 / 最高 / 最低 / 昨收。

use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _};
use gpui_kit::{
    div, prelude::FluentBuilder as _, px, App, FontWeight, IntoElement, ParentElement as _,
    RenderOnce, SharedString, Styled as _, Window,
};

use super::chart::{change_color, fmt_price};

/// 报价摘要。
///
/// 所有字段都是可选的：拿不到数据的品种照样能画出来，只是显示占位符。
#[derive(IntoElement)]
pub struct QuoteSummary {
    /// 顶部标题，如 `伦敦金（现货黄金） · XAU`；`None` 显示"等待行情…"。
    title: Option<SharedString>,
    /// 报价单位，显示在大字价格右侧。
    unit: SharedString,
    price: Option<f64>,
    change: Option<f64>,
    change_pct: Option<f64>,
    /// 一行额外的折算信息，如 `¥900.22/克`。
    note: Option<SharedString>,
    /// 今开 / 最高 / 最低 / 昨收。
    stats: Option<[(&'static str, f64); 4]>,
}

impl Default for QuoteSummary {
    fn default() -> Self {
        Self::new()
    }
}

impl QuoteSummary {
    pub fn new() -> Self {
        Self {
            title: None,
            unit: SharedString::default(),
            price: None,
            change: None,
            change_pct: None,
            note: None,
            stats: None,
        }
    }

    /// 顶部标题；`None` 时显示"等待行情…"。
    pub fn title(mut self, title: Option<impl Into<SharedString>>) -> Self {
        self.title = title.map(Into::into);
        self
    }

    pub fn unit(mut self, unit: impl Into<SharedString>) -> Self {
        self.unit = unit.into();
        self
    }

    pub fn price(mut self, price: Option<f64>) -> Self {
        self.price = price;
        self
    }

    /// 涨跌额与涨跌幅（百分比），决定大字价格的颜色与箭头方向。
    pub fn change(mut self, change: Option<f64>, change_pct: Option<f64>) -> Self {
        self.change = change;
        self.change_pct = change_pct;
        self
    }

    /// 价格行末尾的补充说明，如人民币折算价；`None` 时不显示。
    pub fn note(mut self, note: Option<impl Into<SharedString>>) -> Self {
        self.note = note.map(Into::into);
        self
    }

    /// 今开 / 最高 / 最低 / 昨收；值为 0 的项显示 `--`，`None` 时整组不显示。
    pub fn stats(mut self, stats: Option<[(&'static str, f64); 4]>) -> Self {
        self.stats = stats;
        self
    }
}

impl RenderOnce for QuoteSummary {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let color = change_color(self.change, cx);

        let arrow = match self.change {
            Some(c) if c > 0.0 => "▲",
            Some(c) if c < 0.0 => "▼",
            _ => "—",
        };
        let change_text = match (self.change, self.change_pct) {
            (Some(c), Some(p)) => format!("{arrow} {c:+.2}  ({p:+.2}%)"),
            _ => format!("{arrow} --"),
        };
        let price_text = self
            .price
            .filter(|p| p.is_finite())
            .map(fmt_price)
            .unwrap_or_else(|| "----".to_string());

        v_flex()
            .gap(px(8.))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.title.unwrap_or_else(|| "等待行情…".into())),
            )
            .child(
                h_flex()
                    .gap(px(14.))
                    .items_baseline()
                    .child(
                        div()
                            .text_3xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(color)
                            .child(price_text),
                    )
                    .child(div().text_sm().text_color(muted).child(self.unit))
                    .child(div().text_lg().text_color(color).child(change_text))
                    .when_some(self.note, |row, note| {
                        row.child(div().text_sm().text_color(muted).child(note))
                    })
                    .child(div().flex_1())
                    .children(self.stats.map(|items| {
                        h_flex()
                            .gap(px(16.))
                            .text_sm()
                            .text_color(muted)
                            .children(items.map(|(label, value)| {
                                div().child(format!(
                                    "{label} {}",
                                    if value > 0.0 {
                                        fmt_price(value)
                                    } else {
                                        "--".to_string()
                                    }
                                ))
                            }))
                    })),
            )
    }
}
