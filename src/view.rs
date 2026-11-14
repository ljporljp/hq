//! 视图层：纯渲染，不含取数逻辑。
//!
//! 布局：
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │ 标题 · 刷新 / 主题切换                                │
//! ├──────────┬───────────────────────────────────────────┤
//! │ 品类导航 │ 选中品种的大字报价                          │
//! │ 全部     │ 时间范围工具栏                              │
//! │ 粮食     │ 当前分组的图卡网格（流式换行）               │
//! │ 能源     │                                           │
//! │ …        │                                           │
//! ├──────────┴───────────────────────────────────────────┤
//! │ 状态栏                                                │
//! └──────────────────────────────────────────────────────┘
//! ```
//!
//! 所有可复用的展示元素都在 [`crate::components`] 里，这里只负责把
//! [`MarketView`] 的状态喂给它们。

use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Sizable as _, Theme, ThemeMode,
};
use gpui_kit::{
    div, prelude::FluentBuilder as _, px, Context, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Styled as _, Window,
};

use crate::components::{
    fmt_price, InstrumentCard, NavItem, NavList, QuoteSummary, SegmentedControl, Series,
};
use crate::feed::{Category, Instrument, GRAMS_PER_TROY_OUNCE, INSTRUMENTS};
use crate::model::{cny_per_gram, MarketView, Range};

/// 图卡里曲线区域的高度。30 张图要在一屏里滚动浏览，单张不能太高：
/// 130px 足够看出当天走势和日 K 形态，一屏能放下两行。
const CHART_HEIGHT: f32 = 130.0;

/// 左侧导航的固定宽度。
const NAV_WIDTH: f32 = 132.0;

/// 以美元/金衡盎司报价的贵金属，额外折算成人民币/克。
const USD_PER_OUNCE_SYMBOLS: &[&str] = &["XAU", "XAG", "XPT", "XPD", "GC", "SI"];

impl Render for MarketView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .p(px(20.))
            .gap(px(14.))
            .child(div().flex_shrink_0().child(self.render_header(&theme, cx)))
            .child(
                // 中间区域吃满剩余高度并允许被压缩，内部才有确定的滚动高度。
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap(px(16.))
                    .items_start()
                    .child(div().flex_shrink_0().child(self.render_nav(cx)))
                    .child(
                        // 右侧整列撑满剩余高度，图卡网格才能在内部滚动。
                        v_flex()
                            .flex_1()
                            .self_stretch()
                            .min_h_0()
                            .gap(px(14.))
                            .child(div().flex_shrink_0().child(self.render_selected()))
                            .child(div().flex_shrink_0().child(self.render_toolbar(&theme, cx)))
                            .child(self.render_grid(cx)),
                    ),
            )
            .child(div().flex_shrink_0().child(self.render_footer(&theme)))
    }
}

impl MarketView {
    fn render_header(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let dark = theme.is_dark();
        let (ready, total) = self.loaded_count();
        h_flex()
            .gap(px(12.))
            .child(
                v_flex()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("外盘行情看板"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "数据来源：新浪财经 hq.sinajs.cn · 每 5 秒刷新 · 已加载 {ready}/{total}"
                            )),
                    ),
            )
            .child(div().flex_1())
            .child(
                Button::new("refresh")
                    .ghost()
                    .small()
                    .label("立即刷新")
                    .on_click(cx.listener(|view, _, _, cx| view.refresh_now(cx))),
            )
            .child(
                Button::new("theme")
                    .ghost()
                    .small()
                    .label(if dark { "浅色" } else { "深色" })
                    .on_click(cx.listener(move |_, _, _, cx| {
                        let next = if dark { ThemeMode::Light } else { ThemeMode::Dark };
                        Theme::change(next, None, cx);
                    })),
            )
    }

    /// 左侧品类导航："全部" + 六个分组，行尾带品种数。
    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.category();
        let mut items = vec![NavItem::new("全部").count(INSTRUMENTS.len())];
        items.extend(
            Category::ALL.into_iter().map(|category| {
                NavItem::new(category.label()).count(category.instruments().count())
            }),
        );

        // 下标 0 是"全部"，其余按 `Category::ALL` 的顺序排。
        let selected = match current {
            None => Some(0),
            Some(category) => Category::ALL
                .iter()
                .position(|c| *c == category)
                .map(|index| index + 1),
        };

        NavList::new("category-nav")
            .heading("品类")
            .width(NAV_WIDTH)
            .items(items)
            .selected(selected)
            .on_select(cx.listener(|view, index: &usize, _, cx| {
                let category = index
                    .checked_sub(1)
                    .and_then(|index| Category::ALL.get(index).copied());
                view.select_category(category, cx);
            }))
    }

    /// 顶部：选中品种的最新报价。
    fn render_selected(&self) -> impl IntoElement {
        let instrument = self.selected_instrument();
        let quote = self.selected_quote();

        let title = instrument.map(|it| format!("{} · {}", it.name, it.symbol));
        let unit = instrument.map(|it| it.unit).unwrap_or("");

        // 人民币折算：只有以美元/盎司计价的贵金属才有意义，且需要汇率。
        let note = instrument
            .filter(|it| USD_PER_OUNCE_SYMBOLS.contains(&it.symbol))
            .zip(quote.map(|q| q.price))
            .zip(self.usdcny())
            .map(|((_, price), rate)| format!("¥{:.2}/克", cny_per_gram(price, rate)));

        QuoteSummary::new()
            .title(title)
            .unit(unit)
            .price(quote.map(|q| q.price))
            .change(
                quote.and_then(|q| q.change()),
                quote.and_then(|q| q.change_pct()),
            )
            .note(note)
            .stats(quote.map(|q| {
                [
                    ("今开", q.open),
                    ("最高", q.high),
                    ("最低", q.low),
                    ("昨收", q.prev_close),
                ]
            }))
    }

    fn render_toolbar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.range();
        let selected = Range::ALL.iter().position(|r| *r == current);

        h_flex()
            .gap(px(8.))
            .child(
                SegmentedControl::new("range")
                    .options(Range::ALL.iter().map(|r| r.label().into()))
                    .selected(selected)
                    .on_select(cx.listener(|view, index: &usize, _, cx| {
                        if let Some(range) = Range::ALL.get(*index).copied() {
                            view.select_range(range, cx);
                        }
                    })),
            )
            .child(div().flex_1())
            .when(self.is_loading(), |row| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("加载中…"),
                )
            })
            .when_some(
                self.selected_series().and_then(|s| s.extent()),
                |row, (lo, hi)| {
                    let unit = self.selected_instrument().map(|it| it.unit).unwrap_or("");
                    row.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("区间 {} ~ {} {unit}", fmt_price(lo), fmt_price(hi))),
                    )
                },
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("点击图卡切换顶部品种"),
            )
    }

    /// 流式布局：当前分组下全部品种的迷你曲线，按窗口宽度自动换行。
    fn render_grid(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_index();

        v_flex()
            .flex_1()
            .min_h_0()
            .id("instrument-grid")
            .overflow_y_scrollbar()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(12.))
                    .children(
                        // 遍历整张品种表再按分组过滤：图卡的 id 和点击回调都要用
                        // 品种在整张表里的下标，过滤后仍要能反查到它。
                        INSTRUMENTS
                            .iter()
                            .enumerate()
                            .filter(|(_, it)| self.is_visible(**it))
                            .map(|(index, instrument)| {
                                self.render_card(index, *instrument, selected == Some(index), cx)
                                    .into_any_element()
                            }),
                    ),
            )
    }

    fn render_card(
        &self,
        index: usize,
        instrument: Instrument,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let quote = self.quote_of(instrument);
        let series = self
            .series_of(instrument)
            .cloned()
            .unwrap_or_else(|| Series::Intraday(Vec::new()));
        let empty_text = if self.is_loading() {
            "正在获取行情…"
        } else {
            "暂无数据（可能处于休市时段）"
        };

        InstrumentCard::new(("instrument-card", index), instrument.name)
            .symbol(instrument.symbol)
            .unit(instrument.unit)
            .price(quote.map(|q| q.price))
            .change_pct(quote.and_then(|q| q.change_pct()))
            .series(series)
            .empty_text(empty_text)
            .chart_height(CHART_HEIGHT)
            .selected(selected)
            .on_click(cx.listener(move |view, _, _, cx| view.select_instrument(index, cx)))
    }

    fn render_footer(&self, theme: &Theme) -> impl IntoElement {
        let status = match self.error() {
            Some(err) => div()
                .text_color(theme.chart_bearish)
                .child(format!("⚠ {err}")),
            None => match self.updated_at() {
                Some(at) => div().child(format!("最后更新 {at}")),
                None if self.selected_quote().is_some() => div().child("显示本地缓存"),
                None => div().child("尚未取到数据"),
            },
        };
        // 有缓存回退时明确提示，避免把旧价钱当成实时价。
        let stale = self.is_stale().then(|| {
            div()
                .text_color(theme.chart_1)
                .child("· 含本地缓存，正在重试网络")
        });
        // 存档（FlowDB / SQLite）写失败时也要说出来，否则数据悄悄没落盘。
        let archive = self
            .archive_error()
            .map(|err| format!("· 存档失败：{err}"));        // 报价自带交易所时间，和本机拉取时间分开显示：休市时前者会停住。
        let quote_time = self
            .selected_quote()
            .map(|q| format!("行情时间 {} {}", q.date, q.time));
        let usdcny = self.usdcny().map(|rate| format!("USD/CNY {rate:.4}"));
        h_flex()
            .gap(px(12.))
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(status)
            .when_some(stale, |row, el| row.child(el))
            .when_some(archive, |row, text| {
                row.child(div().text_color(theme.chart_bearish).child(text))
            })
            .when_some(quote_time, |row, text| row.child(div().child(text)))
            .when_some(usdcny, |row, text| row.child(div().child(text)))
            .child(div().flex_1())
            .child(div().child(format!("1 盎司 = {GRAMS_PER_TROY_OUNCE:.4} 克")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::fmt_price;
    use crate::feed::Quote;
    use crate::model::cny_per_gram;

    #[test]
    fn converts_usd_per_ounce_to_cny_per_gram() {
        // 4000 USD/oz，汇率 7.0 → 4000 / 31.1034768 * 7 ≈ 900.2
        let value = cny_per_gram(4000.0, 7.0);
        assert!((value - 900.22).abs() < 0.05, "got {value}");
    }

    #[test]
    fn cny_conversion_only_lists_metals() {
        // 换算表必须都能在品种表里找到，否则界面永远不显示 ¥/克。
        for symbol in USD_PER_OUNCE_SYMBOLS {
            assert!(Instrument::find(symbol).is_some(), "{symbol} 不是有效品种");
        }
    }

    #[test]
    fn quote_change_is_signed_for_cards() {
        let quote = Quote {
            price: 100.0,
            prev_close: 90.0,
            ..Default::default()
        };
        assert_eq!(quote.change(), Some(10.0));
        assert!((quote.change_pct().unwrap() - 11.111_111).abs() < 1e-5);
        assert_eq!(fmt_price(quote.price), "100.0");
    }
}
