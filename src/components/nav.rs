//! 左侧竖向分组导航：每个分组一行，可点击切换，选中行高亮。

use std::rc::Rc;

use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _};
use gpui_kit::{
    div, prelude::FluentBuilder as _, px, App, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, RenderOnce, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window,
};

type SelectHandler = Rc<dyn Fn(&usize, &mut Window, &mut App) + 'static>;

/// 导航里的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavItem {
    pub label: SharedString,
    /// 分组内的品种数，显示在行尾；`None` 不显示。
    pub count: Option<usize>,
}

impl NavItem {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            count: None,
        }
    }

    pub fn count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }
}

/// 竖向分组导航。
#[derive(IntoElement)]
pub struct NavList {
    id: SharedString,
    /// 列表上方的小标题，如"品类"。
    heading: Option<SharedString>,
    items: Vec<NavItem>,
    selected: Option<usize>,
    width: f32,
    on_select: Option<SelectHandler>,
}

impl NavList {
    pub fn new(id: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            heading: None,
            items: Vec::new(),
            selected: None,
            width: 132.0,
            on_select: None,
        }
    }

    pub fn heading(mut self, heading: impl Into<SharedString>) -> Self {
        self.heading = Some(heading.into());
        self
    }

    pub fn items(mut self, items: impl IntoIterator<Item = NavItem>) -> Self {
        self.items = items.into_iter().collect();
        self
    }

    pub fn selected(mut self, selected: Option<usize>) -> Self {
        self.selected = selected;
        self
    }

    pub fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    /// 点击回调，参数是被点中项的下标。
    pub fn on_select(mut self, handler: impl Fn(&usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for NavList {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let selected = self.selected;
        let handler = self.on_select;
        let id = self.id;

        v_flex()
            .w(px(self.width))
            .flex_none()
            .gap(px(2.))
            .when_some(self.heading, |list, heading| {
                list.child(
                    div()
                        .px(px(10.))
                        .pb(px(6.))
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(heading),
                )
            })
            .children(self.items.into_iter().enumerate().map(|(index, item)| {
                let is_selected = selected == Some(index);
                let handler = handler.clone();
                h_flex()
                    .id((id.clone(), index))
                    .px(px(10.))
                    .py(px(6.))
                    .gap(px(8.))
                    .rounded_md()
                    .text_sm()
                    .when(is_selected, |row| {
                        row.bg(theme.accent)
                            .text_color(theme.accent_foreground)
                            .font_weight(FontWeight::MEDIUM)
                    })
                    .when(!is_selected, |row| {
                        row.text_color(theme.muted_foreground)
                            .hover(|style| style.bg(theme.secondary))
                    })
                    .child(div().flex_1().truncate().child(item.label))
                    .when_some(item.count, |row, count| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(count.to_string()),
                        )
                    })
                    .when_some(handler, |row, handler| {
                        row.on_click(move |_, window, cx| handler(&index, window, cx))
                    })
            }))
    }
}
