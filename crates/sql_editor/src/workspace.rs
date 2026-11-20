use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, StyleRefinement,
    Styled as _, Subscription, Window, actions, div, prelude::FluentBuilder as _, px,
};

use crate::connections::{self, SavedConnection};
use crate::query_tab::{ConnectionFields, QueryTab, ToggleVimMode};
use crate::settings;
use crate::themes::{self, SelectTheme};

pub const WORKSPACE_CONTEXT: &str = "Workspace";

actions!(
    sql_editor,
    [
        /// Open a new query tab with the current tab's connection.
        NewTab,
        /// Close the current tab and cancel its running query.
        CloseTab,
        /// Switch to the next tab.
        NextTab,
        /// Switch to the previous tab.
        PreviousTab
    ]
);

struct OpenTab {
    view: Entity<QueryTab>,
    _observation: Subscription,
}

pub struct Workspace {
    tabs: Vec<OpenTab>,
    active_index: usize,
    next_tab_number: usize,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut workspace = Self {
            tabs: Vec::new(),
            active_index: 0,
            next_tab_number: 1,
        };
        let first_connection = connections::names(cx).into_iter().next();
        let fields = ConnectionFields {
            connection: first_connection.clone(),
            database: String::new(),
        };
        workspace.open_tab(fields, window, cx);
        if let (Some(name), Some(tab)) = (first_connection, workspace.active_tab().cloned()) {
            tab.update(cx, |tab, cx| tab.use_connection(&name, window, cx));
        }
        workspace
    }

    fn use_connection_in_active_tab(
        &mut self,
        name: &SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.active_tab().cloned() {
            tab.update(cx, |tab, cx| tab.use_connection(name, window, cx));
            let editor_focus = tab.read(cx).editor_focus_handle(cx);
            editor_focus.focus(window, cx);
        }
    }

    /// Opens the add or edit dialog; `editing` names the connection being edited.
    fn open_connection_dialog(
        editing: Option<SavedConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("BI warehouse")
                .default_value(
                    editing
                        .as_ref()
                        .map(|saved| saved.name.clone())
                        .unwrap_or_default(),
                )
        });
        let string_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Server=tcp:xxxx.datawarehouse.fabric.microsoft.com,1433;Database=...;Authentication=ActiveDirectoryInteractive")
                .default_value(
                    editing
                        .as_ref()
                        .map(|saved| saved.connection_string.clone())
                        .unwrap_or_default(),
                )
        });
        let original_name = editing.map(|saved| saved.name);
        let title = if original_name.is_some() {
            "Edit connection"
        } else {
            "New connection"
        };
        let workspace = cx.weak_entity();
        let name_focus = name_input.focus_handle(cx);
        window.open_dialog(cx, move |dialog, _, _| {
            let name_input = name_input.clone();
            let string_input = string_input.clone();
            let original_name = original_name.clone();
            let workspace = workspace.clone();
            dialog
                .title(title)
                .w(px(640.))
                .child(
                    v_flex()
                        .gap_3()
                        .child(field_label("Name"))
                        .child(Input::new(&name_input))
                        .child(field_label("Connection string"))
                        .child(Input::new(&string_input))
                        .child(div().text_xs().child(
                            "Entra ID sign-in only for now. It uses your az login session.",
                        )),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new()
                                .child(Button::new("cancel").label("Cancel").outline()),
                        )
                        .child(
                            DialogAction::new().child(Button::new("save").label("Save").primary()),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    let name = name_input.read(cx).value().trim().to_owned();
                    let connection_string = string_input.read(cx).value().trim().to_owned();
                    if name.is_empty() {
                        window.push_notification("Give the connection a name.", cx);
                        return false;
                    }
                    if let Err(error) = sql_db::parse_connection_string(&connection_string) {
                        window.push_notification(format!("Connection string: {error}"), cx);
                        return false;
                    }
                    connections::save(
                        cx,
                        SavedConnection {
                            name: name.clone(),
                            connection_string,
                        },
                        original_name.as_deref(),
                    );
                    let name = SharedString::from(name);
                    let updated = workspace.update(cx, |workspace, cx| {
                        workspace.use_connection_in_active_tab(&name, window, cx);
                    });
                    if let Err(error) = updated {
                        tracing::debug!(%error, "workspace closed while the dialog was open");
                    }
                    true
                })
        });
        window.defer(cx, move |window, cx| name_focus.focus(window, cx));
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_connection = self
            .active_tab()
            .and_then(|tab| tab.read(cx).connection_fields(cx).connection);
        let rows = connections::all(cx)
            .iter()
            .enumerate()
            .map(|(index, saved)| {
                let name = SharedString::from(saved.name.clone());
                let is_active = active_connection.as_ref() == Some(&name);
                let editing = saved.clone();
                let removing = saved.name.clone();
                h_flex()
                    .id(SharedString::from(format!("connection-{index}")))
                    .group("connection-row")
                    .justify_between()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .when(is_active, |row| row.bg(cx.theme().accent))
                    .hover(|row| row.bg(cx.theme().list_hover))
                    .child(div().truncate().child(name.clone()))
                    .child(
                        h_flex()
                            .invisible()
                            .group_hover("connection-row", StyleRefinement::visible)
                            .child(
                                Button::new(SharedString::from(format!("edit-connection-{index}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(AssetIcon::SquarePen)
                                    .tooltip("Edit")
                                    .on_click(cx.listener(move |_, _, window, cx| {
                                        cx.stop_propagation();
                                        Self::open_connection_dialog(
                                            Some(editing.clone()),
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!(
                                    "delete-connection-{index}"
                                )))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Delete)
                                .tooltip("Delete")
                                .on_click(cx.listener(
                                    move |_, _, _, cx| {
                                        cx.stop_propagation();
                                        connections::remove(cx, &removing);
                                    },
                                )),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.use_connection_in_active_tab(&name, window, cx);
                    }))
            });
        v_flex()
            .w(px(220.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .text_sm()
            .child(
                h_flex()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("CONNECTIONS")
                    .child(
                        Button::new("add-connection")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip("Add a connection")
                            .on_click(cx.listener(|_, _, window, cx| {
                                Self::open_connection_dialog(None, window, cx);
                            })),
                    ),
            )
            .child(v_flex().px_1().gap_px().children(rows))
            .when(connections::all(cx).is_empty(), |sidebar| {
                sidebar.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Add a connection with +"),
                )
            })
    }

    fn active_tab(&self) -> Option<&Entity<QueryTab>> {
        self.tabs.get(self.active_index).map(|tab| &tab.view)
    }

    fn open_tab(&mut self, fields: ConnectionFields, window: &mut Window, cx: &mut Context<Self>) {
        let number = self.next_tab_number;
        self.next_tab_number += 1;
        let view = cx.new(|cx| QueryTab::new(number, fields, window, cx));
        // Tab titles and the status line come from the tab's state.
        let observation = cx.observe(&view, |_, _, cx| cx.notify());
        self.tabs.push(OpenTab {
            view,
            _observation: observation,
        });
        self.activate(self.tabs.len() - 1, window, cx);
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        self.active_index = index;
        let editor_focus = tab.view.read(cx).editor_focus_handle(cx);
        // Focus after this update so the new tab's editor exists in the element tree.
        window.defer(cx, move |window, cx| editor_focus.focus(window, cx));
        cx.notify();
    }

    fn new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self
            .active_tab()
            .map(|tab| tab.read(cx).connection_fields(cx))
            .unwrap_or_default();
        self.open_tab(fields, window, cx);
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab_at(self.active_index, window, cx);
    }

    fn close_tab_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        let closed = self.tabs.remove(index);
        closed.view.update(cx, QueryTab::stop_running_query);
        if self.tabs.is_empty() {
            let fields = closed.view.read(cx).connection_fields(cx);
            self.open_tab(fields, window, cx);
            return;
        }
        let active = if index < self.active_index || self.active_index >= self.tabs.len() {
            self.active_index.saturating_sub(1)
        } else {
            self.active_index
        };
        self.activate(active, window, cx);
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        let next = (self.active_index + 1) % self.tabs.len().max(1);
        self.activate(next, window, cx);
    }

    fn previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.tabs.len().max(1);
        let previous = (self.active_index + count - 1) % count;
        self.activate(previous, window, cx);
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = self.tabs.iter().enumerate().map(|(index, tab)| {
            let query_tab = tab.view.read(cx);
            let title = if query_tab.is_running() {
                SharedString::from(format!("● {}", query_tab.title(cx)))
            } else {
                query_tab.title(cx)
            };
            Tab::new().label(title).suffix(
                Button::new(SharedString::from(format!("close-tab-{index}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Close tab (Ctrl+W)")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_tab_at(index, window, cx);
                    })),
            )
        });
        TabBar::new("query-tabs")
            .w_full()
            .selected_index(self.active_index)
            .on_click(cx.listener(|this, index: &usize, window, cx| {
                this.activate(*index, window, cx);
            }))
            .children(tabs)
            // gpui-kit draws `last_empty_space` right after the last tab, but only
            // when the bar has a suffix, so the suffix is an empty spacer.
            .last_empty_space(
                div().px_1().child(
                    Button::new("new-tab")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .tooltip("New tab (Ctrl+T)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.new_tab(&NewTab, window, cx);
                        })),
                ),
            )
            .suffix(div())
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, vim_status) = self
            .active_tab()
            .map(|tab| {
                let tab = tab.read(cx);
                (tab.status_text(), tab.vim_status(cx))
            })
            .unwrap_or_default();
        let vim_enabled = settings::get(cx).vim_mode;
        h_flex()
            .justify_between()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(cx.theme().status_bar_border)
            .bg(cx.theme().status_bar)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(
                h_flex()
                    .gap_3()
                    .children(vim_status.map(|mode| {
                        div()
                            .min_w_16()
                            .text_color(cx.theme().foreground)
                            .child(mode)
                    }))
                    .child(status),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("vim-toggle")
                            .ghost()
                            .xsmall()
                            .label(if vim_enabled { "Vim on" } else { "Vim off" })
                            .tooltip("Toggle vim keys")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ToggleVimMode), cx);
                            }),
                    )
                    .child(render_theme_picker(cx)),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .key_context(WORKSPACE_CONTEXT)
            .on_action(cx.listener(Self::new_tab))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_tab_bar(cx))
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .children(self.active_tab().cloned()),
                            ),
                    ),
            )
            .child(self.render_status_bar(cx))
    }
}

fn render_theme_picker(cx: &App) -> impl IntoElement {
    let selected = themes::selected(cx);
    let names = themes::names(cx);
    Button::new("theme-picker")
        .ghost()
        .xsmall()
        .label(selected.clone())
        .tooltip("Theme")
        .dropdown_menu(move |mut menu, _, _| {
            for name in &names {
                menu = menu.menu_with_check(
                    name.clone(),
                    *name == selected,
                    Box::new(SelectTheme(name.clone())),
                );
            }
            menu
        })
}

fn field_label(text: &'static str) -> impl IntoElement {
    div().text_sm().child(text)
}
