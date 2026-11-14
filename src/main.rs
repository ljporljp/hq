//! hq —— 一个 Windows 桌面的外盘行情看板（黄金、白银、原油、农产品、有色金属等）。
//!
//! 技术栈：GPUI（Zed 的 GPU 渲染 UI 框架）+ GPUI Kit（组件库与图表）。
//! 数据：新浪财经公开行情接口（免费、无需 Key），见 [`feed`]。
//!
//! 分层：
//! - [`feed`]  取数与解析（阻塞，只在后台线程调用）
//! - [`cache`] 接口原文的本地缓存，启动时垫底、失败时兜底
//! - [`archive`] 成功取到的行情落进 FlowDB（时序）+ SQLite（查询）
//! - [`model`] 状态与轮询循环（GPUI entity）
//! - [`view`]  渲染

mod archive;
mod cache;
mod components;
mod feed;
mod model;
mod view;

use gpui_kit::assets::Assets;
use gpui_kit::{
    px, size, App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions,
};
use model::MarketView;

fn main() {
    gpui_kit::application()
        .with_assets(Assets)
        .run(|cx: &mut App| {
            gpui_kit::init(cx);

            // 一屏要铺 30 个品种的图卡，默认窗口比原先大一些：1200 宽能排下三列。
            let bounds = Bounds::centered(None, size(px(1200.), px(880.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(720.), px(560.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("外盘行情看板".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };

            gpui_kit::open_window(options, cx, |_, cx| cx.new(MarketView::new))
                .expect("failed to open window");

            cx.activate(true);
        });
}
