use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _, h_flex, h_resizable, resizable_panel,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, actions, div, px,
};

use crate::connection_form::ConnectionForm;
use crate::connections::{self, SavedConnection};
use crate::lsp_servers::{self, LspServers, ServerStatus};
use crate::object_explorer::{ExplorerEvent, ObjectExplorer};
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
    explorer: Entity<ObjectExplorer>,
    _explorer_subscription: Subscription,
    _servers_observation: Subscription,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let explorer = cx.new(|cx| ObjectExplorer::new(window, cx));
        let explorer_subscription = cx.subscribe_in(&explorer, window, Self::on_explorer_event);
        let mut workspace = Self {
            tabs: Vec::new(),
            active_index: 0,
            next_tab_number: 1,
            explorer,
            _explorer_subscription: explorer_subscription,
            // The status line shows the active tab's language server.
            _servers_observation: cx.observe_global::<LspServers>(|_, cx| cx.notify()),
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

    fn on_explorer_event(
        &mut self,
        _: &Entity<ObjectExplorer>,
        event: &ExplorerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ExplorerEvent::UseConnection {
                connection,
                database,
            } => {
                self.use_connection_in_active_tab(connection, window, cx);
                if let (Some(database), Some(tab)) = (database, self.active_tab().cloned()) {
                    tab.update(cx, |tab, cx| tab.set_database(database, window, cx));
                }
            }
            ExplorerEvent::OpenQuery {
                connection,
                database,
                sql,
                run,
            } => {
                self.open_tab(
                    ConnectionFields {
                        connection: Some(connection.clone()),
                        database: database.to_string(),
                    },
                    window,
                    cx,
                );
                if let Some(tab) = self.active_tab().cloned() {
                    tab.update(cx, |tab, cx| {
                        tab.set_database(database, window, cx);
                        tab.set_sql(sql, window, cx);
                        if *run {
                            tab.run_everything(window, cx);
                        }
                    });
                }
            }
            ExplorerEvent::InsertText(text) => {
                if let Some(tab) = self.active_tab().cloned() {
                    tab.update(cx, |tab, cx| tab.insert_text(text, window, cx));
                }
            }
            ExplorerEvent::AddConnection => Self::open_connection_dialog(None, window, cx),
            ExplorerEvent::EditConnection(saved) => {
                Self::open_connection_dialog(Some(saved.clone()), window, cx);
            }
        }
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
        let form = cx.new(|cx| ConnectionForm::new(editing.as_ref(), window, cx));
        let original_name = editing.map(|saved| saved.name);
        let title = if original_name.is_some() {
            "Edit connection"
        } else {
            "New connection"
        };
        let workspace = cx.weak_entity();
        let name_focus = form.focus_handle(cx);
        window.open_dialog(cx, move |dialog, _, _| {
            let form = form.clone();
            let original_name = original_name.clone();
            let workspace = workspace.clone();
            dialog
                .title(title)
                .w(px(640.))
                .child(form.clone())
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
                    let saved = match form.read(cx).saved_connection(cx) {
                        Ok(saved) => saved,
                        Err(message) => {
                            window.push_notification(message, cx);
                            return false;
                        }
                    };
                    if connections::is_unreadable_name(cx, &saved.name) {
                        window.push_notification(
                            "connections.toml has an entry with this name that this version cannot read; choose another name.",
                            cx,
                        );
                        return false;
                    }
                    let name = saved.name.clone();
                    connections::save(cx, saved, original_name.as_deref());
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
        closed.view.update(cx, |tab, cx| {
            tab.stop_running_query(cx);
            tab.release_language_server(cx);
        });
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
                    .tooltip("Close tab (Ctrl+Shift+W)")
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
                        .tooltip("New tab (Ctrl+N)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.new_tab(&NewTab, window, cx);
                        })),
                ),
            )
            .suffix(div())
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, vim_status, connection) = self
            .active_tab()
            .map(|tab| {
                let tab = tab.read(cx);
                (
                    tab.status_text(),
                    tab.vim_status(cx),
                    tab.connection_fields(cx).connection,
                )
            })
            .unwrap_or_default();
        let server_status = connection
            .and_then(|connection| lsp_servers::status(cx, &connection))
            .map(|(command, status)| match status {
                ServerStatus::Starting => format!("{command}: starting"),
                ServerStatus::Running => command,
                ServerStatus::Failed { message, retry_at } => {
                    let wait = retry_at.saturating_duration_since(std::time::Instant::now());
                    format!("{command}: {message}; retry in {}s", wait.as_secs())
                }
            });
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
            .children(server_status.map(|text| div().child(text)))
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
                h_flex().flex_1().min_h_0().child(
                    h_resizable("workspace-split")
                        .child(
                            resizable_panel()
                                .size(px(280.))
                                .size_range(px(180.)..px(600.))
                                .child(
                                    div()
                                        .size_full()
                                        .border_r_1()
                                        .border_color(cx.theme().border)
                                        .child(self.explorer.clone()),
                                ),
                        )
                        .child(
                            resizable_panel().child(
                                v_flex()
                                    .size_full()
                                    .min_w_0()
                                    .child(self.render_tab_bar(cx))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_h_0()
                                            .children(self.active_tab().cloned()),
                                    ),
                            ),
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
