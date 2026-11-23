//! Find in Folder: lists the lines matching a search in every file under the chosen folders.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, px,
    uniform_list,
};

use crate::folders::{self, SearchResults};
use crate::settings;

/// Wait after the last key before searching.
const SEARCH_DELAY: Duration = Duration::from_millis(250);

pub enum FindEvent {
    Open { path: PathBuf, line: usize },
}

impl EventEmitter<FindEvent> for FindInFolder {}

enum Outcome {
    Idle,
    Searching,
    Done(Arc<SearchResults>),
    Failed(SharedString),
}

enum ResultRow {
    File(SharedString),
    Line {
        match_index: usize,
        label: SharedString,
    },
}

pub struct FindInFolder {
    roots: Vec<PathBuf>,
    input: Entity<InputState>,
    is_regex: bool,
    outcome: Outcome,
    rows: Vec<ResultRow>,
    /// Set to stop the search running in the background.
    cancel: Arc<AtomicBool>,
    search_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl FindInFolder {
    pub fn new(roots: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Text to find"));
        let subscriptions = vec![cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.schedule_search(cx);
            }
        })];
        Self {
            roots,
            input,
            is_regex: false,
            outcome: Outcome::Idle,
            rows: Vec::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            search_task: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn focus_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        let pattern = self.input.read(cx).value().to_string();
        if pattern.is_empty() {
            self.search_task = None;
            self.set_outcome(Outcome::Idle, cx);
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        let roots = self.roots.clone();
        let is_regex = self.is_regex;
        let show_all = settings::get(cx).files_show_all;
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DELAY).await;
            if this
                .update(cx, |this, cx| this.set_outcome(Outcome::Searching, cx))
                .is_err()
            {
                return;
            }
            let found = cx
                .background_spawn({
                    let cancel = cancel.clone();
                    async move { folders::search(&roots, &pattern, is_regex, show_all, &cancel) }
                })
                .await;
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let outcome = match found {
                Ok(results) => Outcome::Done(Arc::new(results)),
                Err(error) => Outcome::Failed(format!("Not a valid pattern: {error}").into()),
            };
            if let Err(error) = this.update(cx, |this, cx| this.set_outcome(outcome, cx)) {
                tracing::debug!(%error, "find closed before the search finished");
            }
        }));
    }

    fn set_outcome(&mut self, outcome: Outcome, cx: &mut Context<Self>) {
        self.rows = match &outcome {
            Outcome::Done(results) => self.result_rows(results),
            Outcome::Idle | Outcome::Searching | Outcome::Failed(_) => Vec::new(),
        };
        self.outcome = outcome;
        cx.notify();
    }

    fn result_rows(&self, results: &SearchResults) -> Vec<ResultRow> {
        let mut rows = Vec::new();
        let mut last_file = None;
        for (match_index, found) in results.matches.iter().enumerate() {
            if last_file != Some(&found.path) {
                last_file = Some(&found.path);
                let root = folders::containing_folder(
                    self.roots.iter().map(PathBuf::as_path),
                    &found.path,
                );
                let label = root.map_or_else(
                    || found.path.display().to_string(),
                    |root| folders::relative_path(root, &found.path),
                );
                rows.push(ResultRow::File(label.into()));
            }
            rows.push(ResultRow::Line {
                match_index,
                label: format!("{:>5}  {}", found.line_number, found.line).into(),
            });
        }
        rows
    }

    fn status(&self) -> String {
        match &self.outcome {
            Outcome::Idle => {
                let count = self.roots.len();
                if count == 1 {
                    "Searches every file in the folder.".to_owned()
                } else {
                    format!("Searches every file in {count} folders.")
                }
            }
            Outcome::Searching => "Searching…".to_owned(),
            Outcome::Failed(message) => message.to_string(),
            Outcome::Done(results) => {
                let found = results.matches.len();
                let more = if results.truncated {
                    format!(" (stopped at {})", folders::MAX_SEARCH_MATCHES)
                } else {
                    String::new()
                };
                format!(
                    "{found} matching lines{more} in {} files searched.",
                    results.files_searched
                )
            }
        }
    }

    fn open_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let (Some(ResultRow::Line { match_index, .. }), Outcome::Done(results)) =
            (self.rows.get(index), &self.outcome)
        else {
            return;
        };
        if let Some(found) = results.matches.get(*match_index) {
            cx.emit(FindEvent::Open {
                path: found.path.clone(),
                line: found.line_number,
            });
        }
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        range
            .filter_map(|index| {
                let row = self.rows.get(index)?;
                let element = match row {
                    ResultRow::File(label) => div()
                        .id(("find-row", index))
                        .h(px(24.))
                        .px_2()
                        .pt_1()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .truncate()
                        .child(label.clone()),
                    ResultRow::Line { label, .. } => div()
                        .id(("find-row", index))
                        .h(px(24.))
                        .px_2()
                        .text_sm()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(cx.theme().muted_foreground)
                        .truncate()
                        .hover(|row| row.bg(cx.theme().list_hover))
                        .child(label.clone())
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.open_row(index, cx);
                        })),
                };
                Some(element.into_any_element())
            })
            .collect()
    }
}

impl Drop for FindInFolder {
    // Dropping the task leaves the background search running; the flag stops it.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Focusable for FindInFolder {
    fn focus_handle(&self, cx: &gpui_kit::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for FindInFolder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row_count = self.rows.len();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.input)))
                    .child(
                        Button::new("find-regex")
                            .small()
                            .label(".*")
                            .tooltip("Regular expression")
                            .selected(self.is_regex)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.is_regex = !this.is_regex;
                                this.schedule_search(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.status()),
            )
            .child(
                uniform_list(
                    "find-rows",
                    row_count,
                    cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx)),
                )
                .h(px(420.)),
            )
    }
}
