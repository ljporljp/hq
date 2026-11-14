//! 分段控件：一排互斥按钮，用下标回传选中项。

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{h_flex, Selectable as _, Sizable as _};
use gpui_kit::{
    prelude::FluentBuilder as _, px, App, IntoElement, ParentElement as _, RenderOnce,
    SharedString, Styled as _, Window,
};

type SelectHandler = Rc<dyn Fn(&usize, &mut Window, &mut App) + 'static>;

/// 一排互斥按钮。
///
/// 选中项按**下标**管理，所以文案可以随数据变化（比如带计数的分组名），
/// 也不会因为两个选项文案相同而串台。
#[derive(IntoElement)]
pub struct SegmentedControl {
    /// 按钮 id 前缀，同一屏有多个分段控件时必须区分。
    id: SharedString,
    options: Vec<SharedString>,
    selected: Option<usize>,
    on_select: Option<SelectHandler>,
}

impl SegmentedControl {
    pub fn new(id: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            options: Vec::new(),
            selected: None,
            on_select: None,
        }
    }

    /// 选项文案，顺序即回传的下标。
    pub fn options(mut self, options: impl IntoIterator<Item = SharedString>) -> Self {
        self.options = options.into_iter().collect();
        self
    }

    /// 当前选中项；`None` 表示一个都不选。
    pub fn selected(mut self, selected: Option<usize>) -> Self {
        self.selected = selected;
        self
    }

    /// 点击回调，参数是选中项的下标（与 `cx.listener` 的参数类型一致）。
    pub fn on_select(mut self, handler: impl Fn(&usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for SegmentedControl {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let handler = self.on_select;
        let id = self.id;
        let selected = self.selected;

        h_flex()
            .gap(px(8.))
            .children(self.options.into_iter().enumerate().map(|(index, label)| {
                let handler = handler.clone();
                Button::new((id.clone(), index))
                    .small()
                    .label(label)
                    .when(selected == Some(index), |button| button.primary())
                    .when(selected != Some(index), |button| button.ghost())
                    .selected(selected == Some(index))
                    .when_some(handler, |button, handler| {
                        button.on_click(move |_, window, cx| handler(&index, window, cx))
                    })
            }))
    }
}
