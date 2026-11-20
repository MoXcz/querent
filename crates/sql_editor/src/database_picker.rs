//! The tab's database picker: the connection's databases, filtered as you
//! type, plus a "Use …" row for a name the list does not have.

use gpui_kit::component::IndexPath;
use gpui_kit::component::combobox::ComboboxState;
use gpui_kit::component::label::Label;
use gpui_kit::component::searchable_list::{SearchableListDelegate, SearchableListItem};
use gpui_kit::{App, IntoElement, SharedString, Task, Window};

pub type DatabaseSelect = ComboboxState<DatabaseChoices>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseChoice {
    name: SharedString,
    /// Typed in the search box, not in the server's list.
    typed: bool,
}

impl SearchableListItem for DatabaseChoice {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.name.clone()
    }

    fn render_highlighted(&self, query: &str, _: &mut Window, _: &mut App) -> impl IntoElement {
        if self.typed {
            Label::new(format!("Use “{}”", self.name))
        } else {
            Label::new(self.name.clone()).highlights(query)
        }
    }

    fn value(&self) -> &SharedString {
        &self.name
    }
}

pub struct DatabaseChoices {
    names: Vec<SharedString>,
    shown: Vec<DatabaseChoice>,
}

impl DatabaseChoices {
    /// `names` plus `current` when the list lacks it, so the tab's choice always shows.
    pub fn new(mut names: Vec<SharedString>, current: impl Into<SharedString>) -> Self {
        let current = current.into();
        if !current.is_empty() && !names.contains(&current) {
            names.insert(0, current);
        }
        let shown = names
            .iter()
            .map(|name| DatabaseChoice {
                name: name.clone(),
                typed: false,
            })
            .collect();
        Self { names, shown }
    }
}

impl SearchableListDelegate for DatabaseChoices {
    type Item = DatabaseChoice;

    fn items_count(&self, _: usize) -> usize {
        self.shown.len()
    }

    fn item(&self, index: IndexPath) -> Option<&DatabaseChoice> {
        self.shown.get(index.row)
    }

    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        DatabaseChoice: SearchableListItem<Value = V>,
        V: PartialEq,
    {
        self.shown
            .iter()
            .position(|choice| choice.value() == value)
            .map(IndexPath::new)
    }

    fn perform_search(&mut self, query: &str, _: &mut Window, _: &mut App) -> Task<()> {
        self.shown = filter(&self.names, query);
        Task::ready(())
    }
}

/// Names containing `query`, led by a "Use" row when none matches it exactly.
fn filter(names: &[SharedString], query: &str) -> Vec<DatabaseChoice> {
    let query = query.trim();
    let lowered = query.to_lowercase();
    let mut shown: Vec<DatabaseChoice> = names
        .iter()
        .filter(|name| name.to_lowercase().contains(&lowered))
        .map(|name| DatabaseChoice {
            name: name.clone(),
            typed: false,
        })
        .collect();
    if !query.is_empty() && !names.iter().any(|name| name.eq_ignore_ascii_case(query)) {
        shown.insert(
            0,
            DatabaseChoice {
                name: SharedString::from(query.to_owned()),
                typed: true,
            },
        );
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(choices: &[DatabaseChoice]) -> Vec<(&str, bool)> {
        choices
            .iter()
            .map(|choice| (choice.name.as_ref(), choice.typed))
            .collect()
    }

    #[test]
    fn filters_and_offers_typed_names() {
        let list: Vec<SharedString> = vec!["analytics".into(), "Sales".into(), "sales_old".into()];
        assert_eq!(
            names(&filter(&list, "sal")),
            vec![("sal", true), ("Sales", false), ("sales_old", false)]
        );
        assert_eq!(
            names(&filter(&list, "sales")),
            vec![("Sales", false), ("sales_old", false)]
        );
        assert_eq!(filter(&list, "").len(), 3);
        let with_current = DatabaseChoices::new(list, "archive");
        assert_eq!(
            with_current.names.first().map(AsRef::as_ref),
            Some("archive")
        );
    }
}
