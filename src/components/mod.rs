//! 可复用的视图组件：只负责渲染，不认识 [`crate::model::MarketView`]。
//!
//! 每个组件都是 `RenderOnce`，用 builder 收展示数据与回调，所以同一套组件
//! 既能拼出本项目的行情看板，也能被别的视图直接拿去用：
//!
//! | 组件                | 用途                                     |
//! | ------------------- | ---------------------------------------- |
//! | [`MiniChart`]       | 分时 / 日 K 的迷你图（无坐标轴、无悬浮） |
//! | [`InstrumentCard`]  | 一个品种的图卡：名称、价格、迷你图       |
//! | [`QuoteSummary`]    | 顶部大字报价（价格、涨跌、开高低收）     |
//! | [`SegmentedControl`]| 一排互斥的分段按钮（时间范围切换）       |
//! | [`NavList`]         | 左侧竖向分组导航（品类切换）             |

pub mod card;
pub mod chart;
pub mod nav;
pub mod quote;
pub mod segmented;

pub use card::InstrumentCard;
pub use chart::{change_color, fmt_price, ChartPoint, MiniChart, Series};
pub use nav::{NavItem, NavList};
pub use quote::QuoteSummary;
pub use segmented::SegmentedControl;
