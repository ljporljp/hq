//! 品种图卡：名称、代码、最新价与迷你图，整卡可点。

use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _};
use gpui_kit::{
    div, prelude::FluentBuilder as _, px, App, ClickEvent, ElementId, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, RenderOnce, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window,
};

use super::{change_color, fmt_price, MiniChart, Series};

/// 图卡宽度。固定宽度才能让 `flex_wrap` 稳定地按窗口宽度换行。
pub const CARD_WIDTH: f32 = 300.0;

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// 一个品种的图卡。
#[derive(IntoElement)]
pub struct InstrumentCard {
    id: ElementId,
    name: SharedString,
    symbol: SharedString,
    unit: SharedString,
    series: Series,
    empty_text: SharedString,
    price: Option<f64>,
    change_pct: Option<f64>,
    chart_height: f32,
    selected: bool,
    on_click: Option<ClickHandler>,
}

impl InstrumentCard {
    pub fn new(id: impl Into<ElementId>, name: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            symbol: SharedString::default(),
            unit: SharedString::default(),
            series: Series::Intraday(Vec::new()),
            empty_text: "暂无数据".into(),
            price: None,
            change_pct: None,
            chart_height: 130.0,
            selected: false,
            on_click: None,
        }
    }

    /// 交易所品种代码，显示在名称右侧。
    pub fn symbol(mut self, symbol: impl Into<SharedString>) -> Self {
        self.symbol = symbol.into();
        self
    }

    /// 报价单位，显示在价格行右侧。
    pub fn unit(mut self, unit: impl Into<SharedString>) -> Self {
        self.unit = unit.into();
        self
    }

    pub fn series(mut self, series: Series) -> Self {
        self.series = series;
        self
    }

    /// 没有数据时图区里的提示文案。
    pub fn empty_text(mut self, text: impl Into<SharedString>) -> Self {
        self.empty_text = text.into();
        self
    }

    /// 最新价；`None` 显示 `--`。
    pub fn price(mut self, price: Option<f64>) -> Self {
        self.price = price;
        self
    }

    /// 涨跌幅（百分比），同时决定价格行的颜色。
    pub fn change_pct(mut self, change_pct: Option<f64>) -> Self {
        self.change_pct = change_pct;
        self
    }

    /// 曲线区域高度，必须显式给量，图表自己按 100% 撑满。
    pub fn chart_height(mut self, height: f32) -> Self {
        self.chart_height = height;
        self
    }

    /// 选中态：描一圈主色边框。
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// 整卡点击回调。
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for InstrumentCard {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let color = change_color(self.change_pct, cx);
        let change_text = self
            .change_pct
            .map(|p| format!("{p:+.2}%"))
            .unwrap_or_else(|| "--".to_string());
        let price_text = self
            .price
            .map(fmt_price)
            .unwrap_or_else(|| "--".to_string());

        let card = v_flex()
            .id(self.id.clone())
            .w(px(CARD_WIDTH))
            .flex_none()
            .gap(px(6.))
            .p(px(10.))
            .rounded_lg()
            .bg(theme.secondary)
            .when(self.selected, |card| {
                card.border_2().border_color(theme.chart_1)
            })
            .when(!self.selected, |card| {
                card.border_1().border_color(theme.border)
            })
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .child(self.name),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(self.symbol),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(6.))
                    .items_baseline()
                    .child(div().text_xs().text_color(color).child(price_text))
                    .child(div().text_xs().text_color(color).child(change_text))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(self.unit),
                    ),
            )
            .child(
                MiniChart::new(self.id.clone(), self.series)
                    .height(px(self.chart_height))
                    .empty_text(self.empty_text),
            );

        match self.on_click {
            Some(handler) => card.on_click(move |event, window, cx| handler(event, window, cx)),
            None => card,
        }
    }
}
