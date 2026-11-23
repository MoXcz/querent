//! Built-in and user themes, in gpui-kit's theme JSON format.
//!
//! gpui-kit's `ThemeRegistry::watch_dir` drops themes that were not loaded
//! from its directory on every reload, so this module keeps its own catalog.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::{Theme, ThemeConfig, ThemeSet};
use gpui_kit::{Action, App, Global, SharedString};
use notify::Watcher as _;

use crate::settings;

pub const DEFAULT_THEME: &str = "Solitude";
const BUILT_IN_THEMES: [(&str, &str); 3] = [
    ("solitude.json", include_str!("../themes/solitude.json")),
    ("catppuccin.json", include_str!("../themes/catppuccin.json")),
    ("solarized.json", include_str!("../themes/solarized.json")),
];
/// Editors often write a file in several steps; wait for them to settle.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);

/// Apply the named theme and remember it.
#[derive(Action, Clone, PartialEq)]
#[action(namespace = querent, no_json)]
pub struct SelectTheme(pub SharedString);

struct ThemeCatalog {
    configs: BTreeMap<SharedString, Rc<ThemeConfig>>,
    selected: SharedString,
    themes_dir: Option<PathBuf>,
    _watcher: Option<notify::RecommendedWatcher>,
}

impl Global for ThemeCatalog {}

pub fn init(cx: &mut App) {
    let themes_dir = settings::config_dir(cx).map(|directory| directory.join("themes"));
    let configs = load_themes(themes_dir.as_deref());
    let selected = settings::get(cx)
        .theme
        .clone()
        .map(SharedString::from)
        .filter(|name| configs.contains_key(name))
        .unwrap_or_else(|| DEFAULT_THEME.into());
    let watcher = themes_dir.as_deref().and_then(|directory| {
        watch(directory, cx)
            .inspect_err(|error| tracing::warn!(%error, "theme hot reload is off"))
            .ok()
    });

    cx.set_global(ThemeCatalog {
        configs,
        selected: selected.clone(),
        themes_dir,
        _watcher: watcher,
    });
    apply(&selected, cx);
    cx.on_action(|action: &SelectTheme, cx| select(&action.0, cx));
}

pub fn names(cx: &App) -> Vec<SharedString> {
    cx.global::<ThemeCatalog>()
        .configs
        .keys()
        .cloned()
        .collect()
}

pub fn selected(cx: &App) -> SharedString {
    cx.global::<ThemeCatalog>().selected.clone()
}

pub fn select(name: &SharedString, cx: &mut App) {
    if !cx.global::<ThemeCatalog>().configs.contains_key(name) {
        return;
    }
    cx.global_mut::<ThemeCatalog>().selected = name.clone();
    apply(name, cx);
    settings::update(cx, |settings| settings.theme = Some(name.to_string()));
}

fn apply(name: &SharedString, cx: &mut App) {
    let Some(config) = cx.global::<ThemeCatalog>().configs.get(name).cloned() else {
        return;
    };
    Theme::update(cx, |theme| theme.apply_config(&config));
    cx.refresh_windows();
}

fn reload(cx: &mut App) {
    let themes = cx.global::<ThemeCatalog>();
    let configs = load_themes(themes.themes_dir.as_deref());
    let selected = if configs.contains_key(&themes.selected) {
        themes.selected.clone()
    } else {
        DEFAULT_THEME.into()
    };
    let themes = cx.global_mut::<ThemeCatalog>();
    themes.configs = configs;
    themes.selected = selected.clone();
    apply(&selected, cx);
}

/// Built-in themes first, then user files in name order; a later theme with the same name wins.
fn load_themes(themes_dir: Option<&Path>) -> BTreeMap<SharedString, Rc<ThemeConfig>> {
    let mut configs = BTreeMap::new();
    for (source, text) in BUILT_IN_THEMES {
        insert_theme_set(source, text, &mut configs);
    }
    let Some(themes_dir) = themes_dir else {
        return configs;
    };
    let entries = match std::fs::read_dir(themes_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return configs,
        Err(error) => {
            tracing::warn!(path = %themes_dir.display(), %error, "cannot read themes directory");
            return configs;
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    for path in paths {
        match std::fs::read_to_string(&path) {
            Ok(text) => insert_theme_set(&path.display().to_string(), &text, &mut configs),
            Err(error) => tracing::warn!(path = %path.display(), %error, "cannot read theme"),
        }
    }
    configs
}

fn insert_theme_set(
    source: &str,
    text: &str,
    configs: &mut BTreeMap<SharedString, Rc<ThemeConfig>>,
) {
    match serde_json::from_str::<ThemeSet>(text) {
        Ok(theme_set) => {
            for theme in theme_set.themes {
                configs.insert(theme.name.clone(), Rc::new(theme));
            }
        }
        Err(error) => tracing::warn!(source, %error, "ignoring invalid theme file"),
    }
}

fn watch(themes_dir: &Path, cx: &mut App) -> notify::Result<notify::RecommendedWatcher> {
    std::fs::create_dir_all(themes_dir)?;
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok_and(|event| !event.kind.is_access()) && sender.send(()).is_err() {
            tracing::debug!("theme watcher outlived the app");
        }
    })?;
    watcher.watch(themes_dir, notify::RecursiveMode::NonRecursive)?;

    cx.spawn(async move |cx| {
        while receiver.recv().await.is_some() {
            cx.background_executor().timer(RELOAD_DEBOUNCE).await;
            while receiver.try_recv().is_ok() {}
            cx.update(reload);
        }
    })
    .detach();
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_themes_parse() {
        let configs = load_themes(None);
        let solitude = configs.get(DEFAULT_THEME).expect("Solitude is built in");
        assert!(solitude.mode.is_dark());
        assert!(solitude.highlight.is_some());
        for name in [
            "Catppuccin Latte",
            "Catppuccin Frappé",
            "Catppuccin Macchiato",
            "Catppuccin Mocha",
            "Solarized Dark",
            "Solarized Light",
        ] {
            assert!(configs.contains_key(name), "{name} is built in");
        }
    }

    #[test]
    fn user_theme_overrides_built_in_with_same_name() {
        let directory = std::env::temp_dir().join(format!("querent-themes-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let custom = r##"{"name":"x","themes":[{"name":"Solitude","mode":"light","colors":{"background":"#ffffff"}}]}"##;
        std::fs::write(directory.join("custom.json"), custom).unwrap();
        std::fs::write(directory.join("broken.json"), "{").unwrap();

        let configs = load_themes(Some(&directory));
        std::fs::remove_dir_all(&directory).unwrap();

        assert!(!configs[DEFAULT_THEME].mode.is_dark());
    }

    /// gpui-kit ignores unknown keys, so a typo would silently fall back to a default color.
    #[test]
    fn built_in_themes_use_only_known_keys() {
        for (source, text) in BUILT_IN_THEMES {
            let written: serde_json::Value = serde_json::from_str(text).unwrap();
            let parsed =
                serde_json::to_value(serde_json::from_str::<ThemeSet>(text).unwrap()).unwrap();
            for (index, theme) in written["themes"].as_array().unwrap().iter().enumerate() {
                let known = &parsed["themes"][index];
                for section in ["colors", "highlight"] {
                    for key in theme[section].as_object().unwrap().keys() {
                        assert!(
                            !known[section][key].is_null(),
                            "{source}: unknown {section} key {key}"
                        );
                    }
                }
                for key in theme["highlight"]["syntax"].as_object().unwrap().keys() {
                    assert!(
                        !known["highlight"]["syntax"][key].is_null(),
                        "{source}: unknown syntax key {key}"
                    );
                }
            }
        }
    }
}
