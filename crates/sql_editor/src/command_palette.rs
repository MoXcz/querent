//! The command palette (`ctrl-shift-p`) and the settings panel it can open.

use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::setting::{
    SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::{WindowExt as _, h_flex};
use gpui_kit::{
    Action, App, AppContext as _, FocusHandle, ParentElement as _, SharedString, Styled as _,
    Window, actions, div, px,
};

use crate::query_tab::{CancelQuery, ExportCsv, FormatSql, RunAll, RunQuery, ToggleVimMode};
use crate::settings;
use crate::themes::{self, SelectTheme};
use crate::workspace::{AddConnection, CloseTab, NewTab, NextTab, PreviousTab};

actions!(
    sql_editor,
    [
        /// Search and run any command.
        OpenCommandPalette,
        /// Open the settings panel.
        OpenSettings,
        /// Switch the object explorer between schema folders and one flat list.
        ToggleExplorerGrouping
    ]
);

pub fn init(cx: &mut App) {
    cx.on_action(|_: &ToggleExplorerGrouping, cx| {
        settings::update(cx, |settings| {
            settings.explorer_group_by_schema = !settings.explorer_group_by_schema;
        });
        cx.refresh_windows();
    });
}

struct PaletteCommand {
    group: &'static str,
    label: SharedString,
    keywords: Vec<&'static str>,
    action: Box<dyn Action>,
}

fn command(
    group: &'static str,
    label: impl Into<SharedString>,
    keywords: &[&'static str],
    action: impl Action,
) -> PaletteCommand {
    PaletteCommand {
        group,
        label: label.into(),
        keywords: keywords.to_vec(),
        action: Box::new(action),
    }
}

fn commands(cx: &App) -> Vec<PaletteCommand> {
    let mut commands = vec![
        command("Query", "Run statement", &["execute", "query"], RunQuery),
        command("Query", "Run all", &["execute", "script"], RunAll),
        command("Query", "Cancel query", &["stop"], CancelQuery),
        command("Query", "Export result as CSV", &["save", "csv"], ExportCsv),
        command("Query", "Format SQL", &["pretty", "indent"], FormatSql),
        command("Tabs", "New tab", &["open"], NewTab),
        command("Tabs", "Close tab", &[], CloseTab),
        command("Tabs", "Next tab", &[], NextTab),
        command("Tabs", "Previous tab", &[], PreviousTab),
        command(
            "Connections",
            "Add connection",
            &["new", "database"],
            AddConnection,
        ),
        command(
            "Settings",
            if settings::get(cx).vim_mode {
                "Vim mode: turn off"
            } else {
                "Vim mode: turn on"
            },
            &["toggle", "keys"],
            ToggleVimMode,
        ),
        command(
            "Settings",
            if settings::get(cx).explorer_group_by_schema {
                "Explorer: show a flat list"
            } else {
                "Explorer: group by schema"
            },
            &["tree", "schema"],
            ToggleExplorerGrouping,
        ),
        command(
            "Settings",
            "Open settings",
            &["preferences", "options"],
            OpenSettings,
        ),
    ];
    let selected = themes::selected(cx);
    for name in themes::names(cx) {
        let label = if name == selected {
            format!("Theme: {name} (current)")
        } else {
            format!("Theme: {name}")
        };
        commands.push(command(
            "Themes",
            label,
            &["color", "appearance"],
            SelectTheme(name),
        ));
    }
    commands
}

/// Runs the chosen command after the dialog closes, so tab commands reach the tab.
pub fn open_command_palette(window: &mut Window, cx: &mut App) {
    let previous_focus = window.focused(cx);
    let commands = commands(cx);
    let state = cx.new(|cx| CommandState::new(window, cx));
    let groups = group_items(&commands);
    let actions: Vec<Vec<Box<dyn Action>>> = groups
        .iter()
        .map(|(_, members)| {
            members
                .iter()
                .filter_map(|index| {
                    commands
                        .get(*index)
                        .map(|command| command.action.boxed_clone())
                })
                .collect()
        })
        .collect();
    let group_titles: Vec<(&'static str, Vec<CommandItem>)> = groups
        .into_iter()
        .map(|(title, members)| {
            let items = members
                .into_iter()
                .filter_map(|index| commands.get(index))
                .map(|command| palette_item(command, previous_focus.clone()))
                .collect();
            (title, items)
        })
        .collect();
    let focus_state = state.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let mut palette = Command::new(&state)
            .placeholder("Type a command")
            .max_h(px(420.));
        for (title, items) in &group_titles {
            palette = palette.group(
                CommandGroup::new()
                    .label(*title)
                    .items(items.iter().cloned()),
            );
        }
        let actions = actions
            .iter()
            .map(|group| {
                group
                    .iter()
                    .map(|action| action.boxed_clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let previous_focus = previous_focus.clone();
        let palette = palette.on_confirm(move |index, window, cx| {
            let Some(action) = actions
                .get(index.section)
                .and_then(|group| group.get(index.row))
                .map(|action| action.boxed_clone())
            else {
                return;
            };
            window.close_dialog(cx);
            let previous_focus = previous_focus.clone();
            window.defer(cx, move |window, cx| {
                if let Some(focus) = previous_focus {
                    focus.focus(window, cx);
                }
                window.dispatch_action(action, cx);
            });
        });
        dialog.w(px(560.)).p_0().child(palette)
    });
    window.defer(cx, move |window, cx| {
        focus_state.update(cx, |state, cx| state.focus(window, cx));
    });
}

/// Command indexes by group, in first-seen group order.
fn group_items(commands: &[PaletteCommand]) -> Vec<(&'static str, Vec<usize>)> {
    let mut groups: Vec<(&'static str, Vec<usize>)> = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        match groups.iter_mut().find(|(group, _)| *group == command.group) {
            Some((_, members)) => members.push(index),
            None => groups.push((command.group, vec![index])),
        }
    }
    groups
}

/// A row with the command's label and, when it has one, its key binding as
/// seen from where focus was before the palette opened.
fn palette_item(command: &PaletteCommand, focus: Option<FocusHandle>) -> CommandItem {
    let label = command.label.clone();
    let action = command.action.boxed_clone();
    CommandItem::new()
        .label(label.clone())
        .keywords(command.keywords.iter().copied())
        .child(move |window, _| {
            let binding = match &focus {
                Some(focus) => Kbd::binding_for_action_in(&*action, focus, window),
                None => Kbd::binding_for_action(&*action, None, window),
            };
            h_flex()
                .w_full()
                .justify_between()
                .gap_2()
                .child(div().child(label.clone()))
                .children(binding)
        })
}

/// Settings that change while the panel is open apply at once and save to `settings.toml`.
pub fn open_settings(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme_options: Vec<(SharedString, SharedString)> = themes::names(cx)
            .into_iter()
            .map(|name| (name.clone(), name))
            .collect();
        let settings_view = Settings::new("settings").page(
            SettingPage::new("General")
                .default_open(true)
                .group(
                    SettingGroup::new().title("Appearance").item(
                        SettingItem::new(
                            "Theme",
                            SettingField::dropdown(theme_options, themes::selected, |name, cx| {
                                themes::select(&name, cx);
                            }),
                        )
                        .description(
                            "Built-in Solitude, or a theme from ~/.config/sql-editor/themes.",
                        ),
                    ),
                )
                .group(
                    SettingGroup::new().title("Editor").item(
                        SettingItem::new(
                            "Vim mode",
                            SettingField::switch(
                                |cx| settings::get(cx).vim_mode,
                                |enabled, cx| {
                                    settings::update(cx, |settings| settings.vim_mode = enabled);
                                    cx.refresh_windows();
                                },
                            ),
                        )
                        .description("Normal, insert and visual modes in the SQL editor."),
                    ),
                )
                .group(
                    SettingGroup::new().title("Object explorer").item(
                        SettingItem::new(
                            "Group by schema",
                            SettingField::switch(
                                |cx| settings::get(cx).explorer_group_by_schema,
                                |enabled, cx| {
                                    settings::update(cx, |settings| {
                                        settings.explorer_group_by_schema = enabled;
                                    });
                                    cx.refresh_windows();
                                },
                            ),
                        )
                        .description("Schema folders, or schema.name in one list."),
                    ),
                ),
        );
        dialog
            .title("Settings")
            .w(px(720.))
            .h(px(480.))
            .child(settings_view)
    });
}
