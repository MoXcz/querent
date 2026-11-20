//! Vim emulation as a pure state machine: keys and a text snapshot in, edits out.
//!
//! The editor widget stays the source of truth for text and cursor. Each key
//! gets the current text and cursor, and returns edits for the caller to apply
//! in order. Offsets are UTF-8 byte offsets.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Visual,
    VisualLine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Escape,
    Enter,
    Backspace,
}

#[derive(Debug, Clone, Copy)]
pub struct Buffer<'a> {
    pub text: &'a str,
    pub cursor: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Replace `range` with `text`.
    Replace {
        range: Range<usize>,
        text: String,
    },
    /// Select `range`; an empty range places the caret. With `reversed`, the caret is at the start.
    Select {
        range: Range<usize>,
        reversed: bool,
    },
    Undo,
    Redo,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// False means the editor should handle the key as usual.
    pub handled: bool,
    pub edits: Vec<Edit>,
}

impl Outcome {
    fn handled(edits: Vec<Edit>) -> Self {
        Self {
            handled: true,
            edits,
        }
    }

    fn passed() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Delete,
    Change,
    Yank,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FindKind {
    /// `f`: onto the next match.
    Forward,
    /// `t`: just before the next match.
    TillForward,
    /// `F`: onto the previous match.
    Backward,
    /// `T`: just after the previous match.
    TillBackward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Motion {
    Left,
    Right,
    Down,
    Up,
    NextLineStart,
    WordStart { big: bool },
    WordBack { big: bool },
    WordEnd { big: bool },
    LineStart,
    FirstNonBlank,
    LineEnd,
    FirstLine,
    LastLine,
    Find { kind: FindKind, target: char },
    RepeatFind { reverse: bool },
    ParagraphForward,
    ParagraphBack,
    HalfPageDown,
    HalfPageUp,
    SearchNext { reverse: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextObject {
    Word { big: bool },
    Pair { open: char, close: char },
    Quote(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Motion(Motion),
    Object {
        object: TextObject,
        around: bool,
    },
    /// `dd`, `cc`, `yy`.
    Lines,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Move(Motion),
    Operate {
        operator: Operator,
        target: Target,
    },
    /// `iw`, `a(` and the like in visual mode.
    SelectObject {
        object: TextObject,
        around: bool,
    },
    PasteAfter,
    PasteBefore,
    Undo,
    Redo,
    Insert,
    Append,
    InsertAtLineStart,
    AppendAtLineEnd,
    OpenBelow,
    OpenAbove,
    ToggleVisual,
    ToggleVisualLine,
    SwapVisualEnds,
    JoinLines,
    ToggleCase,
    ReplaceChar(char),
    Repeat,
    StartSearch,
    SearchWordUnderCursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Command {
    count: Option<usize>,
    action: Action,
}

enum Parse {
    Incomplete,
    Invalid,
    Done(Command),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MotionKind {
    Exclusive,
    Inclusive,
    Linewise,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Register {
    text: String,
    linewise: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    keys: Vec<Key>,
    inserted: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Visual {
    anchor: usize,
    head: usize,
}

const HALF_PAGE_LINES: usize = 15;

#[derive(Debug)]
pub struct Vim {
    mode: Mode,
    pending: Vec<Key>,
    register: Option<Register>,
    visual: Option<Visual>,
    /// Column `j`/`k` aim for, kept while moving vertically.
    goal_column: Option<usize>,
    last_cursor: Option<usize>,
    last_find: Option<(FindKind, char)>,
    last_search: Option<String>,
    search_input: Option<String>,
    last_change: Option<Change>,
    /// Keys of the change that entered insert mode, finished on Escape.
    recording: Option<Vec<Key>>,
    insert_start: usize,
    replaying: bool,
    /// The count typed for the command being run, before defaulting to 1.
    typed_count: Option<usize>,
}

impl Default for Vim {
    fn default() -> Self {
        Self {
            mode: Mode::Normal,
            pending: Vec::new(),
            register: None,
            visual: None,
            goal_column: None,
            last_cursor: None,
            last_find: None,
            last_search: None,
            search_input: None,
            last_change: None,
            recording: None,
            insert_start: 0,
            replaying: false,
            typed_count: None,
        }
    }
}

impl Vim {
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    #[must_use]
    pub fn mode_label(&self) -> &'static str {
        match self.mode {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
        }
    }

    /// Keys typed so far for an unfinished command, or the search being typed.
    #[must_use]
    pub fn pending_text(&self) -> String {
        if let Some(search) = &self.search_input {
            return format!("/{search}");
        }
        self.pending.iter().map(|key| key_label(*key)).collect()
    }

    pub fn handle_key(&mut self, key: Key, buffer: Buffer<'_>) -> Outcome {
        if self.last_cursor != Some(buffer.cursor) {
            self.goal_column = None;
        }
        let outcome = match self.mode {
            Mode::Insert => self.handle_insert_key(key, buffer),
            Mode::Normal | Mode::Visual | Mode::VisualLine => {
                if self.search_input.is_some() {
                    self.handle_search_key(key, buffer)
                } else {
                    self.handle_command_key(key, buffer)
                }
            }
        };
        self.last_cursor = final_caret(&outcome.edits).or(self.last_cursor);
        outcome
    }

    fn handle_insert_key(&mut self, key: Key, buffer: Buffer<'_>) -> Outcome {
        if !matches!(key, Key::Escape | Key::Ctrl('[')) {
            return Outcome::passed();
        }
        if let Some(keys) = self.recording.take() {
            let inserted = buffer
                .text
                .get(self.insert_start..buffer.cursor)
                .unwrap_or_default()
                .to_owned();
            self.last_change = Some(Change {
                keys,
                inserted: Some(inserted),
            });
        }
        self.mode = Mode::Normal;
        let line_start = line_start(buffer.text, buffer.cursor);
        let cursor = if buffer.cursor > line_start {
            prev_boundary(buffer.text, buffer.cursor)
        } else {
            buffer.cursor
        };
        Outcome::handled(vec![caret(cursor)])
    }

    fn handle_search_key(&mut self, key: Key, buffer: Buffer<'_>) -> Outcome {
        let Some(search) = self.search_input.as_mut() else {
            return Outcome::passed();
        };
        match key {
            Key::Char(character) => {
                search.push(character);
                Outcome::handled(Vec::new())
            }
            Key::Backspace => {
                if search.pop().is_none() {
                    self.search_input = None;
                }
                Outcome::handled(Vec::new())
            }
            Key::Escape | Key::Ctrl('[' | 'c') => {
                self.search_input = None;
                Outcome::handled(Vec::new())
            }
            Key::Enter => {
                let pattern = self.search_input.take().unwrap_or_default();
                if !pattern.is_empty() {
                    self.last_search = Some(pattern);
                }
                let command = Command {
                    count: None,
                    action: Action::Move(Motion::SearchNext { reverse: false }),
                };
                Outcome::handled(self.execute(command, buffer))
            }
            Key::Ctrl(_) => Outcome::handled(Vec::new()),
        }
    }

    fn handle_command_key(&mut self, key: Key, buffer: Buffer<'_>) -> Outcome {
        if matches!(key, Key::Escape | Key::Ctrl('[')) {
            self.pending.clear();
            if self.visual.is_some() {
                return Outcome::handled(self.exit_visual(buffer.text));
            }
            return Outcome::handled(Vec::new());
        }
        if key == Key::Ctrl('w') {
            // Vim's window prefix; there are no windows here, and the editor's own
            // ctrl-w would delete a word outside insert mode.
            self.pending.clear();
            return Outcome::handled(Vec::new());
        }
        if let Key::Ctrl(character) = key
            && !matches!(character, 'r' | 'd' | 'u')
        {
            // Leave app shortcuts such as ctrl-enter to the app.
            return Outcome::passed();
        }
        self.pending.push(key);
        match parse(&self.pending, self.visual.is_some()) {
            Parse::Incomplete => Outcome::handled(Vec::new()),
            Parse::Invalid => {
                self.pending.clear();
                Outcome::handled(Vec::new())
            }
            Parse::Done(command) => {
                let keys = std::mem::take(&mut self.pending);
                let edits = self.execute(command, buffer);
                if !self.replaying && is_change(command.action) {
                    if self.mode == Mode::Insert {
                        self.recording = Some(keys);
                    } else if self.visual.is_none() {
                        self.last_change = Some(Change {
                            keys,
                            inserted: None,
                        });
                    }
                }
                Outcome::handled(edits)
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per vim command; splitting would scatter the dispatch table"
    )]
    fn execute(&mut self, command: Command, buffer: Buffer<'_>) -> Vec<Edit> {
        let count = command.count.unwrap_or(1).max(1);
        self.typed_count = command.count;
        let text = buffer.text;
        let cursor = self.visual.map_or(buffer.cursor, |visual| visual.head);
        match command.action {
            Action::Move(motion) => {
                let Some((target, _)) = self.motion_target(motion, count, text, cursor) else {
                    return Vec::new();
                };
                if let Some(visual) = self.visual.as_mut() {
                    visual.head = target;
                    return vec![self.visual_selection(text)];
                }
                vec![caret(clamp_normal(text, target))]
            }
            Action::Operate { operator, target } => {
                if self.visual.is_some() {
                    let (range, linewise) = self.visual_range(text);
                    self.visual = None;
                    self.mode = Mode::Normal;
                    return self.apply_operator(operator, range, linewise, text);
                }
                let Some((range, linewise)) =
                    self.operator_range(operator, target, count, text, cursor)
                else {
                    return Vec::new();
                };
                self.apply_operator(operator, range, linewise, text)
            }
            Action::SelectObject { object, around } => {
                let Some(range) = text_object(object, around, text, cursor) else {
                    return Vec::new();
                };
                if range.is_empty() {
                    return Vec::new();
                }
                self.visual = Some(Visual {
                    anchor: range.start,
                    head: prev_boundary(text, range.end),
                });
                self.mode = Mode::Visual;
                vec![self.visual_selection(text)]
            }
            Action::PasteAfter | Action::PasteBefore => {
                self.paste(command.action == Action::PasteAfter, count, text, cursor)
            }
            Action::Undo => vec![Edit::Undo; count],
            Action::Redo => vec![Edit::Redo; count],
            Action::Insert => self.enter_insert(cursor, Vec::new()),
            Action::Append => {
                let at = if cursor < line_end(text, cursor) {
                    next_boundary(text, cursor)
                } else {
                    cursor
                };
                self.enter_insert(at, Vec::new())
            }
            Action::InsertAtLineStart => {
                self.enter_insert(first_non_blank(text, cursor), Vec::new())
            }
            Action::AppendAtLineEnd => self.enter_insert(line_end(text, cursor), Vec::new()),
            Action::OpenBelow => {
                let end = line_end(text, cursor);
                let indent = indentation(text, cursor);
                let inserted = format!("\n{indent}");
                let at = end + inserted.len();
                self.enter_insert(at, vec![replace(end..end, inserted)])
            }
            Action::OpenAbove => {
                let start = line_start(text, cursor);
                let indent = indentation(text, cursor).to_owned();
                let at = start + indent.len();
                self.enter_insert(at, vec![replace(start..start, format!("{indent}\n"))])
            }
            Action::ToggleVisual | Action::ToggleVisualLine => {
                let wanted = if command.action == Action::ToggleVisual {
                    Mode::Visual
                } else {
                    Mode::VisualLine
                };
                if self.mode == wanted {
                    return self.exit_visual(text);
                }
                if self.visual.is_none() {
                    self.visual = Some(Visual {
                        anchor: cursor,
                        head: cursor,
                    });
                }
                self.mode = wanted;
                vec![self.visual_selection(text)]
            }
            Action::SwapVisualEnds => {
                if let Some(visual) = self.visual.as_mut() {
                    std::mem::swap(&mut visual.anchor, &mut visual.head);
                }
                vec![self.visual_selection(text)]
            }
            Action::JoinLines => join_lines(text, cursor, count.max(2) - 1),
            Action::ToggleCase => {
                if let Some(visual) = self.visual {
                    let (range, _) = self.visual_range(text);
                    self.visual = None;
                    self.mode = Mode::Normal;
                    let start = range.start.min(visual.anchor.min(visual.head));
                    return toggle_case(text, range, start);
                }
                let line_end = line_end(text, cursor);
                let mut end = cursor;
                for _ in 0..count {
                    if end < line_end {
                        end = next_boundary(text, end);
                    }
                }
                let next = clamp_normal(text, end);
                toggle_case(text, cursor..end, next)
            }
            Action::ReplaceChar(character) => {
                let line_end = line_end(text, cursor);
                let mut end = cursor;
                for _ in 0..count {
                    if end >= line_end {
                        return Vec::new();
                    }
                    end = next_boundary(text, end);
                }
                let replacement: String = std::iter::repeat_n(character, count).collect();
                let last = cursor + replacement.len() - character.len_utf8();
                vec![replace(cursor..end, replacement), caret(last)]
            }
            Action::Repeat => self.repeat(count, buffer),
            Action::StartSearch => {
                self.search_input = Some(String::new());
                Vec::new()
            }
            Action::SearchWordUnderCursor => {
                let Some(range) = text_object(TextObject::Word { big: false }, false, text, cursor)
                else {
                    return Vec::new();
                };
                let word = text.get(range).unwrap_or_default();
                if word.trim().is_empty() {
                    return Vec::new();
                }
                self.last_search = Some(word.to_owned());
                self.execute(
                    Command {
                        count: command.count,
                        action: Action::Move(Motion::SearchNext { reverse: false }),
                    },
                    buffer,
                )
            }
        }
    }

    fn enter_insert(&mut self, at: usize, mut edits: Vec<Edit>) -> Vec<Edit> {
        self.visual = None;
        self.mode = Mode::Insert;
        self.insert_start = at;
        edits.push(caret(at));
        edits
    }

    fn exit_visual(&mut self, text: &str) -> Vec<Edit> {
        let head = self.visual.take().map_or(0, |visual| visual.head);
        self.mode = Mode::Normal;
        vec![caret(clamp_normal(text, head))]
    }

    fn visual_range(&self, text: &str) -> (Range<usize>, bool) {
        let Some(visual) = self.visual else {
            return (0..0, false);
        };
        let start = visual.anchor.min(visual.head);
        let end = visual.anchor.max(visual.head);
        if self.mode == Mode::VisualLine {
            (
                line_start(text, start)..line_end_with_newline(text, end),
                true,
            )
        } else {
            (start..next_boundary(text, end).min(text.len()), false)
        }
    }

    fn visual_selection(&self, text: &str) -> Edit {
        let (range, _) = self.visual_range(text);
        let reversed = self
            .visual
            .is_some_and(|visual| visual.head < visual.anchor);
        Edit::Select { range, reversed }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per vim command; splitting would scatter the dispatch table"
    )]
    fn motion_target(
        &mut self,
        motion: Motion,
        count: usize,
        text: &str,
        cursor: usize,
    ) -> Option<(usize, MotionKind)> {
        let mut position = cursor;
        let kind = motion_kind(motion);
        match motion {
            Motion::Down | Motion::Up | Motion::HalfPageDown | Motion::HalfPageUp => {
                let lines = if matches!(motion, Motion::HalfPageDown | Motion::HalfPageUp) {
                    HALF_PAGE_LINES * count
                } else {
                    count
                };
                let goal = *self
                    .goal_column
                    .get_or_insert_with(|| column_of(text, cursor));
                let line = line_index(text, cursor);
                let target_line = if matches!(motion, Motion::Down | Motion::HalfPageDown) {
                    (line + lines).min(line_count(text) - 1)
                } else {
                    line.saturating_sub(lines)
                };
                let start = offset_of_line(text, target_line);
                position = offset_at_column(text, start, goal);
                return Some((position, kind));
            }
            Motion::FirstLine | Motion::LastLine => {
                let last = line_count(text) - 1;
                let line = match (motion, self.count_given(count)) {
                    (_, Some(number)) => number.saturating_sub(1).min(last),
                    (Motion::FirstLine, None) => 0,
                    _ => last,
                };
                let start = offset_of_line(text, line);
                return Some((first_non_blank(text, start), kind));
            }
            Motion::RepeatFind { reverse } => {
                let (find_kind, target) = self.last_find?;
                let find_kind = if reverse {
                    reverse_find(find_kind)
                } else {
                    find_kind
                };
                for _ in 0..count {
                    position = find_in_line(text, position, find_kind, target, true)?;
                }
                return Some((
                    position,
                    motion_kind(Motion::Find {
                        kind: find_kind,
                        target,
                    }),
                ));
            }
            Motion::Find {
                kind: find_kind,
                target,
            } => {
                self.last_find = Some((find_kind, target));
                for _ in 0..count {
                    position = find_in_line(text, position, find_kind, target, false)?;
                }
                return Some((position, kind));
            }
            Motion::SearchNext { reverse } => {
                let pattern = self.last_search.clone()?;
                for _ in 0..count {
                    position = search(text, position, &pattern, reverse)?;
                }
                return Some((position, kind));
            }
            Motion::Left
            | Motion::Right
            | Motion::NextLineStart
            | Motion::WordStart { .. }
            | Motion::WordBack { .. }
            | Motion::WordEnd { .. }
            | Motion::LineStart
            | Motion::FirstNonBlank
            | Motion::LineEnd
            | Motion::ParagraphForward
            | Motion::ParagraphBack => {}
        }
        for _ in 0..count {
            position = match motion {
                Motion::Left => {
                    if position > line_start(text, position) {
                        prev_boundary(text, position)
                    } else {
                        position
                    }
                }
                Motion::Right => {
                    let end = line_end(text, position);
                    if position < end {
                        next_boundary(text, position)
                    } else {
                        position
                    }
                }
                Motion::NextLineStart => {
                    let end = line_end(text, position);
                    if end < text.len() {
                        first_non_blank(text, end + 1)
                    } else {
                        position
                    }
                }
                Motion::WordStart { big } => word_start_forward(text, position, big),
                Motion::WordBack { big } => word_start_backward(text, position, big),
                Motion::WordEnd { big } => word_end_forward(text, position, big),
                Motion::LineStart => line_start(text, position),
                Motion::FirstNonBlank => first_non_blank(text, position),
                Motion::LineEnd => {
                    let end = line_end(text, position);
                    if end > line_start(text, position) {
                        prev_boundary(text, end)
                    } else {
                        end
                    }
                }
                Motion::ParagraphForward => paragraph_forward(text, position),
                Motion::ParagraphBack => paragraph_back(text, position),
                Motion::Down
                | Motion::Up
                | Motion::HalfPageDown
                | Motion::HalfPageUp
                | Motion::FirstLine
                | Motion::LastLine
                | Motion::Find { .. }
                | Motion::RepeatFind { .. }
                | Motion::SearchNext { .. } => position,
            };
        }
        Some((position, kind))
    }

    /// `gg` and `G` read the count as a line number only when one was typed.
    fn count_given(&self, _count: usize) -> Option<usize> {
        self.typed_count
    }

    fn operator_range(
        &mut self,
        operator: Operator,
        target: Target,
        count: usize,
        text: &str,
        cursor: usize,
    ) -> Option<(Range<usize>, bool)> {
        match target {
            Target::Lines => {
                let last_line = line_count(text) - 1;
                let first = line_index(text, cursor);
                let last = (first + count - 1).min(last_line);
                let start = offset_of_line(text, first);
                let end_line_start = offset_of_line(text, last);
                if operator == Operator::Change {
                    // `cc` keeps the line break and the indentation.
                    let content_start = first_non_blank(text, start);
                    return Some((content_start..line_end(text, end_line_start), false));
                }
                Some((start..line_end_with_newline(text, end_line_start), true))
            }
            Target::Object { object, around } => {
                text_object(object, around, text, cursor).map(|range| (range, false))
            }
            Target::Motion(motion) => {
                // `cw` acts like `ce` on a word, as in vim.
                let motion = if let Motion::WordStart { big } = motion
                    && operator == Operator::Change
                    && char_at(text, cursor).is_some_and(|c| !c.is_whitespace())
                {
                    Motion::WordEnd { big }
                } else {
                    motion
                };
                let (mut target, kind) = self.motion_target(motion, count, text, cursor)?;
                if let Motion::WordEnd { .. } = motion
                    && operator == Operator::Change
                    && cursor_on_word_end(text, cursor)
                    && count == 1
                {
                    target = cursor;
                }
                if matches!(motion, Motion::WordStart { .. })
                    && line_index(text, target) > line_index(text, cursor)
                {
                    // `dw` on the last word of a line stops at the line end.
                    target = line_end(text, cursor);
                }
                let start = cursor.min(target);
                let end = cursor.max(target);
                match kind {
                    MotionKind::Exclusive => Some((start..end, false)),
                    MotionKind::Inclusive => {
                        Some((start..next_boundary(text, end).min(text.len()), false))
                    }
                    MotionKind::Linewise => Some((
                        line_start(text, start)..line_end_with_newline(text, end),
                        true,
                    )),
                }
            }
        }
    }

    fn apply_operator(
        &mut self,
        operator: Operator,
        range: Range<usize>,
        linewise: bool,
        text: &str,
    ) -> Vec<Edit> {
        let mut yanked = text.get(range.clone()).unwrap_or_default().to_owned();
        if linewise && !yanked.ends_with('\n') {
            yanked.push('\n');
        }
        self.register = Some(Register {
            text: yanked,
            linewise,
        });
        match operator {
            Operator::Yank => vec![caret(range.start)],
            Operator::Delete => {
                let mut range = range;
                // Deleting the last line also removes the line break before it.
                if linewise && range.end == text.len() && !text.ends_with('\n') && range.start > 0 {
                    range.start -= 1;
                }
                let remaining = format!(
                    "{}{}",
                    text.get(..range.start).unwrap_or_default(),
                    text.get(range.end..).unwrap_or_default()
                );
                let cursor = if linewise {
                    let start = range.start.min(remaining.len());
                    let line = if range.start > 0 && range.end == text.len() {
                        line_start(&remaining, start.saturating_sub(1))
                    } else {
                        start
                    };
                    first_non_blank(&remaining, line)
                } else {
                    clamp_normal(&remaining, range.start)
                };
                vec![replace(range, String::new()), caret(cursor)]
            }
            Operator::Change => {
                let start = range.start;
                self.enter_insert(start, vec![replace(range, String::new())])
            }
        }
    }

    fn paste(&mut self, after: bool, count: usize, text: &str, cursor: usize) -> Vec<Edit> {
        let Some(register) = self.register.clone() else {
            return Vec::new();
        };
        let content = register.text.repeat(count);
        if let Some(visual) = self.visual {
            let (range, _) = self.visual_range(text);
            self.visual = None;
            self.mode = Mode::Normal;
            let at = range.start.min(visual.anchor);
            let end = at + content.len();
            return vec![replace(range, content), caret(prev_boundary_in(end, at))];
        }
        if register.linewise {
            let at = if after {
                line_end_with_newline(text, cursor)
            } else {
                line_start(text, cursor)
            };
            // Pasting below a last line that has no line break needs one first.
            if after && at == text.len() && !text.ends_with('\n') {
                let body = content.trim_end_matches('\n');
                let inserted = format!("\n{body}");
                let cursor = at + 1 + leading_blank_len(body);
                return vec![replace(at..at, inserted), caret(cursor)];
            }
            let cursor = at + leading_blank_len(&content);
            return vec![replace(at..at, content), caret(cursor)];
        }
        let at = if after && cursor < line_end(text, cursor) {
            next_boundary(text, cursor)
        } else {
            cursor
        };
        let end = at + content.len();
        vec![replace(at..at, content), caret(prev_boundary_in(end, at))]
    }

    fn repeat(&mut self, count: usize, buffer: Buffer<'_>) -> Vec<Edit> {
        let Some(change) = self.last_change.clone() else {
            return Vec::new();
        };
        let mut text = buffer.text.to_owned();
        let mut cursor = buffer.cursor;
        let mut edits = Vec::new();
        self.replaying = true;
        for _ in 0..count {
            let Parse::Done(command) = parse(&change.keys, false) else {
                break;
            };
            let step = self.execute(
                command,
                Buffer {
                    text: &text,
                    cursor,
                },
            );
            (text, cursor) = apply_edits(&text, cursor, &step);
            edits.extend(step);
            if self.mode == Mode::Insert {
                let inserted = change.inserted.clone().unwrap_or_default();
                let step = vec![
                    replace(cursor..cursor, inserted.clone()),
                    caret(cursor + inserted.len()),
                ];
                (text, cursor) = apply_edits(&text, cursor, &step);
                edits.extend(step);
                self.mode = Mode::Normal;
                if cursor > line_start(&text, cursor) {
                    cursor = prev_boundary(&text, cursor);
                }
                edits.push(caret(cursor));
            }
        }
        self.replaying = false;
        edits
    }
}

/// Applies edits to a copy of `text`; returns the new text and caret.
#[must_use]
pub fn apply_edits(text: &str, cursor: usize, edits: &[Edit]) -> (String, usize) {
    let mut text = text.to_owned();
    let mut cursor = cursor;
    for edit in edits {
        match edit {
            Edit::Replace { range, text: new } => {
                let start = range.start.min(text.len());
                let end = range.end.clamp(start, text.len());
                text.replace_range(start..end, new);
                cursor = start + new.len();
            }
            Edit::Select { range, reversed } => {
                cursor = if *reversed { range.start } else { range.end };
            }
            Edit::Undo | Edit::Redo => {}
        }
    }
    (text, cursor)
}

#[expect(
    clippy::too_many_lines,
    reason = "one arm per vim command; splitting would scatter the dispatch table"
)]
fn parse(keys: &[Key], visual: bool) -> Parse {
    let mut index = 0;
    let count = parse_count(keys, &mut index);
    let Some(&key) = keys.get(index) else {
        return Parse::Incomplete;
    };
    index += 1;
    let done = |action| Parse::Done(Command { count, action });

    if let Some(operator) = operator_for(key) {
        if visual {
            return done(Action::Operate {
                operator,
                target: Target::Lines,
            });
        }
        let inner_count = parse_count(keys, &mut index);
        let count = match (count, inner_count) {
            (None, None) => None,
            (outer, inner) => Some(outer.unwrap_or(1) * inner.unwrap_or(1)),
        };
        let Some(&next) = keys.get(index) else {
            return Parse::Incomplete;
        };
        index += 1;
        if next == key {
            return Parse::Done(Command {
                count,
                action: Action::Operate {
                    operator,
                    target: Target::Lines,
                },
            });
        }
        let target = match next {
            Key::Char(scope @ ('i' | 'a')) => {
                let Some(&object_key) = keys.get(index) else {
                    return Parse::Incomplete;
                };
                let Some(object) = text_object_for(object_key) else {
                    return Parse::Invalid;
                };
                Target::Object {
                    object,
                    around: scope == 'a',
                }
            }
            Key::Char(_) | Key::Ctrl(_) | Key::Escape | Key::Enter | Key::Backspace => {
                match parse_motion(next, keys.get(index..).unwrap_or_default()) {
                    MotionParse::Done(motion) => Target::Motion(motion),
                    MotionParse::Incomplete => return Parse::Incomplete,
                    MotionParse::Invalid => return Parse::Invalid,
                }
            }
        };
        return Parse::Done(Command {
            count,
            action: Action::Operate { operator, target },
        });
    }

    let rest = keys.get(index..).unwrap_or_default();
    let shorthand = |operator, motion| {
        done(Action::Operate {
            operator,
            target: Target::Motion(motion),
        })
    };
    match key {
        Key::Char('x') if visual => done(Action::Operate {
            operator: Operator::Delete,
            target: Target::Lines,
        }),
        Key::Char('x') => shorthand(Operator::Delete, Motion::Right),
        Key::Char('X') => shorthand(Operator::Delete, Motion::Left),
        Key::Char('D') => shorthand(Operator::Delete, Motion::LineEnd),
        Key::Char('C') => shorthand(Operator::Change, Motion::LineEnd),
        Key::Char('s') if visual => done(Action::Operate {
            operator: Operator::Change,
            target: Target::Lines,
        }),
        Key::Char('s') => shorthand(Operator::Change, Motion::Right),
        Key::Char('S') => done(Action::Operate {
            operator: Operator::Change,
            target: Target::Lines,
        }),
        Key::Char('Y') => done(Action::Operate {
            operator: Operator::Yank,
            target: Target::Lines,
        }),
        Key::Char('p') => done(Action::PasteAfter),
        Key::Char('P') => done(Action::PasteBefore),
        Key::Char('u') if !visual => done(Action::Undo),
        Key::Ctrl('r') => done(Action::Redo),
        Key::Char('i') if !visual => done(Action::Insert),
        Key::Char('a') if !visual => done(Action::Append),
        Key::Char(scope @ ('i' | 'a')) => {
            let Some(&object_key) = rest.first() else {
                return Parse::Incomplete;
            };
            match text_object_for(object_key) {
                Some(object) => done(Action::SelectObject {
                    object,
                    around: scope == 'a',
                }),
                None => Parse::Invalid,
            }
        }
        Key::Char('I') => done(Action::InsertAtLineStart),
        Key::Char('A') => done(Action::AppendAtLineEnd),
        Key::Char('o') if visual => done(Action::SwapVisualEnds),
        Key::Char('o') => done(Action::OpenBelow),
        Key::Char('O') => done(Action::OpenAbove),
        Key::Char('v') => done(Action::ToggleVisual),
        Key::Char('V') => done(Action::ToggleVisualLine),
        Key::Char('J') => done(Action::JoinLines),
        Key::Char('~') => done(Action::ToggleCase),
        Key::Char('.') => done(Action::Repeat),
        Key::Char('/') => done(Action::StartSearch),
        Key::Char('*') => done(Action::SearchWordUnderCursor),
        Key::Char('r') => match rest.first() {
            None => Parse::Incomplete,
            Some(Key::Char(character)) => done(Action::ReplaceChar(*character)),
            Some(_) => Parse::Invalid,
        },
        Key::Char(_) | Key::Ctrl(_) | Key::Escape | Key::Enter | Key::Backspace => {
            match parse_motion(key, rest) {
                MotionParse::Done(motion) => done(Action::Move(motion)),
                MotionParse::Incomplete => Parse::Incomplete,
                MotionParse::Invalid => Parse::Invalid,
            }
        }
    }
}

fn parse_count(keys: &[Key], index: &mut usize) -> Option<usize> {
    let mut count: Option<usize> = None;
    while let Some(Key::Char(digit)) = keys.get(*index) {
        let Some(value) = digit.to_digit(10) else {
            break;
        };
        // A leading 0 is the line-start motion, not a count.
        if value == 0 && count.is_none() {
            break;
        }
        count = Some(
            count
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(value as usize),
        );
        *index += 1;
    }
    count
}

fn operator_for(key: Key) -> Option<Operator> {
    match key {
        Key::Char('d') => Some(Operator::Delete),
        Key::Char('c') => Some(Operator::Change),
        Key::Char('y') => Some(Operator::Yank),
        Key::Char(_) | Key::Ctrl(_) | Key::Escape | Key::Enter | Key::Backspace => None,
    }
}

fn text_object_for(key: Key) -> Option<TextObject> {
    let Key::Char(character) = key else {
        return None;
    };
    Some(match character {
        'w' => TextObject::Word { big: false },
        'W' => TextObject::Word { big: true },
        '(' | ')' | 'b' => TextObject::Pair {
            open: '(',
            close: ')',
        },
        '[' | ']' => TextObject::Pair {
            open: '[',
            close: ']',
        },
        '{' | '}' | 'B' => TextObject::Pair {
            open: '{',
            close: '}',
        },
        '<' | '>' => TextObject::Pair {
            open: '<',
            close: '>',
        },
        '\'' | '"' | '`' => TextObject::Quote(character),
        _ => return None,
    })
}

enum MotionParse {
    Done(Motion),
    Incomplete,
    Invalid,
}

fn parse_motion(key: Key, rest: &[Key]) -> MotionParse {
    let motion = match key {
        Key::Char('h') | Key::Backspace => Motion::Left,
        Key::Char('l' | ' ') => Motion::Right,
        Key::Char('j') => Motion::Down,
        Key::Char('k') => Motion::Up,
        Key::Enter | Key::Char('+') => Motion::NextLineStart,
        Key::Char('w') => Motion::WordStart { big: false },
        Key::Char('W') => Motion::WordStart { big: true },
        Key::Char('b') => Motion::WordBack { big: false },
        Key::Char('B') => Motion::WordBack { big: true },
        Key::Char('e') => Motion::WordEnd { big: false },
        Key::Char('E') => Motion::WordEnd { big: true },
        Key::Char('0') => Motion::LineStart,
        Key::Char('^') => Motion::FirstNonBlank,
        Key::Char('$') => Motion::LineEnd,
        Key::Char('G') => Motion::LastLine,
        Key::Char('}') => Motion::ParagraphForward,
        Key::Char('{') => Motion::ParagraphBack,
        Key::Char(';') => Motion::RepeatFind { reverse: false },
        Key::Char(',') => Motion::RepeatFind { reverse: true },
        Key::Char('n') => Motion::SearchNext { reverse: false },
        Key::Char('N') => Motion::SearchNext { reverse: true },
        Key::Ctrl('d') => Motion::HalfPageDown,
        Key::Ctrl('u') => Motion::HalfPageUp,
        Key::Char('g') => {
            return match rest.first() {
                None => MotionParse::Incomplete,
                Some(Key::Char('g')) => MotionParse::Done(Motion::FirstLine),
                Some(_) => MotionParse::Invalid,
            };
        }
        Key::Char(finder @ ('f' | 't' | 'F' | 'T')) => {
            let kind = match finder {
                'f' => FindKind::Forward,
                't' => FindKind::TillForward,
                'F' => FindKind::Backward,
                _ => FindKind::TillBackward,
            };
            return match rest.first() {
                None => MotionParse::Incomplete,
                Some(Key::Char(target)) => MotionParse::Done(Motion::Find {
                    kind,
                    target: *target,
                }),
                Some(_) => MotionParse::Invalid,
            };
        }
        Key::Char(_) | Key::Ctrl(_) | Key::Escape => return MotionParse::Invalid,
    };
    MotionParse::Done(motion)
}

fn motion_kind(motion: Motion) -> MotionKind {
    match motion {
        Motion::Down
        | Motion::Up
        | Motion::NextLineStart
        | Motion::FirstLine
        | Motion::LastLine
        | Motion::HalfPageDown
        | Motion::HalfPageUp => MotionKind::Linewise,
        Motion::WordEnd { .. }
        | Motion::LineEnd
        | Motion::Find {
            kind: FindKind::Forward | FindKind::TillForward,
            ..
        } => MotionKind::Inclusive,
        Motion::Left
        | Motion::Right
        | Motion::WordStart { .. }
        | Motion::WordBack { .. }
        | Motion::LineStart
        | Motion::FirstNonBlank
        | Motion::Find { .. }
        | Motion::RepeatFind { .. }
        | Motion::ParagraphForward
        | Motion::ParagraphBack
        | Motion::SearchNext { .. } => MotionKind::Exclusive,
    }
}

fn is_change(action: Action) -> bool {
    match action {
        Action::Operate { operator, .. } => operator != Operator::Yank,
        Action::PasteAfter
        | Action::PasteBefore
        | Action::Insert
        | Action::Append
        | Action::InsertAtLineStart
        | Action::AppendAtLineEnd
        | Action::OpenBelow
        | Action::OpenAbove
        | Action::JoinLines
        | Action::ToggleCase
        | Action::ReplaceChar(_) => true,
        Action::Move(_)
        | Action::SelectObject { .. }
        | Action::Undo
        | Action::Redo
        | Action::ToggleVisual
        | Action::ToggleVisualLine
        | Action::SwapVisualEnds
        | Action::Repeat
        | Action::StartSearch
        | Action::SearchWordUnderCursor => false,
    }
}

fn reverse_find(kind: FindKind) -> FindKind {
    match kind {
        FindKind::Forward => FindKind::Backward,
        FindKind::Backward => FindKind::Forward,
        FindKind::TillForward => FindKind::TillBackward,
        FindKind::TillBackward => FindKind::TillForward,
    }
}

fn key_label(key: Key) -> String {
    match key {
        Key::Char(character) => character.to_string(),
        Key::Ctrl(character) => format!("^{character}"),
        Key::Escape => "<esc>".to_owned(),
        Key::Enter => "<cr>".to_owned(),
        Key::Backspace => "<bs>".to_owned(),
    }
}

fn caret(offset: usize) -> Edit {
    Edit::Select {
        range: offset..offset,
        reversed: false,
    }
}

fn replace(range: Range<usize>, text: String) -> Edit {
    Edit::Replace { range, text }
}

fn final_caret(edits: &[Edit]) -> Option<usize> {
    edits.iter().rev().find_map(|edit| match edit {
        Edit::Select { range, reversed } if range.is_empty() || *reversed => Some(range.start),
        Edit::Select { range, .. } => Some(range.end),
        Edit::Replace { .. } | Edit::Undo | Edit::Redo => None,
    })
}

// Text helpers. All take and return byte offsets on char boundaries.

fn char_at(text: &str, offset: usize) -> Option<char> {
    text.get(offset..).and_then(|rest| rest.chars().next())
}

fn next_boundary(text: &str, offset: usize) -> usize {
    char_at(text, offset).map_or(text.len(), |character| offset + character.len_utf8())
}

fn prev_boundary(text: &str, offset: usize) -> usize {
    text.get(..offset)
        .and_then(|before| before.char_indices().next_back())
        .map_or(0, |(index, _)| index)
}

/// Start of the last character of a just-inserted span `[start, end)`.
fn prev_boundary_in(end: usize, start: usize) -> usize {
    if end > start { end - 1 } else { start }
}

fn line_start(text: &str, offset: usize) -> usize {
    text.get(..offset)
        .and_then(|before| before.rfind('\n'))
        .map_or(0, |index| index + 1)
}

fn line_end(text: &str, offset: usize) -> usize {
    text.get(offset..)
        .and_then(|rest| rest.find('\n'))
        .map_or(text.len(), |index| offset + index)
}

fn line_end_with_newline(text: &str, offset: usize) -> usize {
    let end = line_end(text, offset);
    if end < text.len() { end + 1 } else { end }
}

fn line_index(text: &str, offset: usize) -> usize {
    text.get(..offset)
        .map_or(0, |before| before.matches('\n').count())
}

fn line_count(text: &str) -> usize {
    text.matches('\n').count() + 1
}

fn offset_of_line(text: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    text.match_indices('\n')
        .nth(line - 1)
        .map_or(text.len(), |(index, _)| index + 1)
}

fn column_of(text: &str, offset: usize) -> usize {
    text.get(line_start(text, offset)..offset)
        .map_or(0, |before| before.chars().count())
}

/// Offset at `column` characters into the line, clamped onto its last character.
fn offset_at_column(text: &str, line_start: usize, column: usize) -> usize {
    let end = line_end(text, line_start);
    let line = text.get(line_start..end).unwrap_or_default();
    let last = line
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index);
    line.char_indices()
        .nth(column)
        .map_or(line_start + last, |(index, _)| line_start + index)
}

fn first_non_blank(text: &str, offset: usize) -> usize {
    let start = line_start(text, offset);
    let end = line_end(text, offset);
    let line = text.get(start..end).unwrap_or_default();
    start + leading_blank_len(line)
}

fn leading_blank_len(text: &str) -> usize {
    text.len() - text.trim_start_matches([' ', '\t']).len()
}

fn indentation(text: &str, offset: usize) -> &str {
    let start = line_start(text, offset);
    let end = line_end(text, offset);
    let line = text.get(start..end).unwrap_or_default();
    line.get(..leading_blank_len(line)).unwrap_or_default()
}

/// Normal mode keeps the cursor on a character, never after the last one.
fn clamp_normal(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let start = line_start(text, offset);
    let end = line_end(text, offset);
    if offset >= end && end > start {
        prev_boundary(text, end)
    } else {
        offset
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Blank,
    Word,
    Punctuation,
}

fn class(character: char, big: bool) -> CharClass {
    if character.is_whitespace() {
        CharClass::Blank
    } else if big || character.is_alphanumeric() || character == '_' {
        CharClass::Word
    } else {
        CharClass::Punctuation
    }
}

fn word_start_forward(text: &str, offset: usize, big: bool) -> usize {
    let Some(start_char) = char_at(text, offset) else {
        return text.len();
    };
    let mut position = offset;
    let start_class = class(start_char, big);
    if start_class != CharClass::Blank {
        while let Some(character) = char_at(text, position) {
            if class(character, big) != start_class {
                break;
            }
            position = next_boundary(text, position);
        }
    }
    while let Some(character) = char_at(text, position) {
        if !character.is_whitespace() {
            break;
        }
        let next = next_boundary(text, position);
        // An empty line counts as a word.
        if character == '\n' && char_at(text, next) == Some('\n') {
            return next;
        }
        position = next;
    }
    position
}

fn word_end_forward(text: &str, offset: usize, big: bool) -> usize {
    let mut position = next_boundary(text, offset);
    while char_at(text, position).is_some_and(char::is_whitespace) {
        position = next_boundary(text, position);
    }
    let Some(character) = char_at(text, position) else {
        return prev_boundary(text, text.len());
    };
    let word_class = class(character, big);
    loop {
        let next = next_boundary(text, position);
        match char_at(text, next) {
            Some(next_char) if class(next_char, big) == word_class => position = next,
            _ => return position,
        }
    }
}

fn word_start_backward(text: &str, offset: usize, big: bool) -> usize {
    if offset == 0 {
        return 0;
    }
    let mut position = prev_boundary(text, offset);
    while position > 0 && char_at(text, position).is_some_and(char::is_whitespace) {
        position = prev_boundary(text, position);
    }
    let Some(character) = char_at(text, position) else {
        return 0;
    };
    let word_class = class(character, big);
    while position > 0 {
        let previous = prev_boundary(text, position);
        match char_at(text, previous) {
            Some(previous_char) if class(previous_char, big) == word_class => {
                position = previous;
            }
            _ => break,
        }
    }
    position
}

fn cursor_on_word_end(text: &str, offset: usize) -> bool {
    let Some(character) = char_at(text, offset) else {
        return false;
    };
    let next = char_at(text, next_boundary(text, offset));
    next.is_none_or(|next| class(next, false) != class(character, false))
}

fn is_blank_line(text: &str, offset: usize) -> bool {
    let start = line_start(text, offset);
    text.get(start..line_end(text, offset))
        .is_some_and(|line| line.trim().is_empty())
}

fn paragraph_forward(text: &str, offset: usize) -> usize {
    let mut position = line_end(text, offset);
    // Skip blank lines right after the cursor, then stop at the next one.
    let mut seen_text = !is_blank_line(text, offset);
    while position < text.len() {
        let next_line = position + 1;
        let blank = is_blank_line(text, next_line);
        if blank && seen_text {
            return next_line;
        }
        seen_text |= !blank;
        position = line_end(text, next_line);
    }
    text.len()
}

fn paragraph_back(text: &str, offset: usize) -> usize {
    let mut start = line_start(text, offset);
    let mut seen_text = !is_blank_line(text, offset);
    while start > 0 {
        let previous_line = line_start(text, start - 1);
        let blank = is_blank_line(text, previous_line);
        if blank && seen_text {
            return previous_line;
        }
        seen_text |= !blank;
        start = previous_line;
    }
    0
}

fn find_in_line(
    text: &str,
    offset: usize,
    kind: FindKind,
    target: char,
    repeating: bool,
) -> Option<usize> {
    let start = line_start(text, offset);
    let end = line_end(text, offset);
    match kind {
        FindKind::Forward | FindKind::TillForward => {
            let mut from = next_boundary(text, offset);
            // A repeated `t` would match the character it already stops before.
            if kind == FindKind::TillForward && repeating {
                from = next_boundary(text, from);
            }
            let found = from + text.get(from..end)?.find(target)?;
            Some(if kind == FindKind::TillForward {
                prev_boundary(text, found)
            } else {
                found
            })
        }
        FindKind::Backward | FindKind::TillBackward => {
            let mut until = offset;
            if kind == FindKind::TillBackward && repeating {
                until = prev_boundary(text, until);
            }
            let found = start + text.get(start..until)?.rfind(target)?;
            Some(if kind == FindKind::TillBackward {
                next_boundary(text, found)
            } else {
                found
            })
        }
    }
}

/// Plain-text search that wraps around; case-insensitive unless the pattern has a capital.
fn search(text: &str, offset: usize, pattern: &str, reverse: bool) -> Option<usize> {
    let case_sensitive = pattern.chars().any(char::is_uppercase);
    let matches: Vec<usize> = if case_sensitive {
        text.match_indices(pattern)
            .map(|(index, _)| index)
            .collect()
    } else {
        let lowered = text.to_lowercase();
        // Lowercasing can change byte lengths; fall back to exact matches then.
        if lowered.len() == text.len() {
            lowered
                .match_indices(&pattern.to_lowercase())
                .map(|(index, _)| index)
                .collect()
        } else {
            text.match_indices(pattern)
                .map(|(index, _)| index)
                .collect()
        }
    };
    if reverse {
        matches
            .iter()
            .rev()
            .find(|&&index| index < offset)
            .or_else(|| matches.last())
            .copied()
    } else {
        matches
            .iter()
            .find(|&&index| index > offset)
            .or_else(|| matches.first())
            .copied()
    }
}

fn text_object(
    object: TextObject,
    around: bool,
    text: &str,
    cursor: usize,
) -> Option<Range<usize>> {
    match object {
        TextObject::Word { big } => {
            let character = char_at(text, cursor)?;
            let word_class = class(character, big);
            let line_start = line_start(text, cursor);
            let line_end = line_end(text, cursor);
            let mut start = cursor;
            while start > line_start {
                let previous = prev_boundary(text, start);
                if char_at(text, previous).is_some_and(|c| class(c, big) == word_class) {
                    start = previous;
                } else {
                    break;
                }
            }
            let mut end = cursor;
            while end < line_end && char_at(text, end).is_some_and(|c| class(c, big) == word_class)
            {
                end = next_boundary(text, end);
            }
            if around && word_class != CharClass::Blank {
                let mut trailing = end;
                while trailing < line_end
                    && char_at(text, trailing).is_some_and(char::is_whitespace)
                {
                    trailing = next_boundary(text, trailing);
                }
                if trailing > end {
                    end = trailing;
                } else {
                    while start > line_start
                        && char_at(text, prev_boundary(text, start))
                            .is_some_and(char::is_whitespace)
                    {
                        start = prev_boundary(text, start);
                    }
                }
            }
            Some(start..end)
        }
        TextObject::Pair { open, close } => {
            let (open_at, close_at) = enclosing_pair(text, cursor, open, close)?;
            if around {
                Some(open_at..close_at + close.len_utf8())
            } else {
                Some(open_at + open.len_utf8()..close_at)
            }
        }
        TextObject::Quote(quote) => {
            let start = line_start(text, cursor);
            let end = line_end(text, cursor);
            let positions: Vec<usize> = text
                .get(start..end)?
                .match_indices(quote)
                .map(|(index, _)| start + index)
                .collect();
            let pairs = || {
                positions
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&[open_at, close_at]| (open_at, close_at))
            };
            let (open_at, close_at) = pairs()
                .find(|&(open_at, close_at)| open_at <= cursor && cursor <= close_at)
                .or_else(|| pairs().find(|&(open_at, _)| open_at > cursor))?;
            if around {
                Some(open_at..close_at + quote.len_utf8())
            } else {
                Some(open_at + quote.len_utf8()..close_at)
            }
        }
    }
}

fn enclosing_pair(text: &str, cursor: usize, open: char, close: char) -> Option<(usize, usize)> {
    let open_at = if char_at(text, cursor) == Some(open) {
        cursor
    } else {
        let mut depth = 0_usize;
        let mut found = None;
        for (index, character) in text.get(..cursor)?.char_indices().rev() {
            if character == close {
                depth += 1;
            } else if character == open {
                if depth == 0 {
                    found = Some(index);
                    break;
                }
                depth -= 1;
            }
        }
        found?
    };
    let mut depth = 0_usize;
    let after_open = open_at + open.len_utf8();
    for (index, character) in text.get(after_open..)?.char_indices() {
        if character == open {
            depth += 1;
        } else if character == close {
            if depth == 0 {
                return Some((open_at, after_open + index));
            }
            depth -= 1;
        }
    }
    None
}

fn join_lines(text: &str, cursor: usize, joins: usize) -> Vec<Edit> {
    let mut edits = Vec::new();
    let mut current = text.to_owned();
    let mut position = cursor;
    for _ in 0..joins {
        let end = line_end(&current, position);
        if end >= current.len() {
            break;
        }
        let next_start = end + 1;
        let next_content = first_non_blank(&current, next_start);
        let separator = if next_content == line_end(&current, next_start)
            || current
                .get(..end)
                .is_some_and(|before| before.ends_with(' '))
        {
            ""
        } else {
            " "
        };
        let edit = replace(end..next_content, separator.to_owned());
        (current, _) = apply_edits(&current, position, std::slice::from_ref(&edit));
        edits.push(edit);
        position = end;
    }
    edits.push(caret(position));
    edits
}

fn toggle_case(text: &str, range: Range<usize>, cursor_after: usize) -> Vec<Edit> {
    let Some(original) = text.get(range.clone()) else {
        return Vec::new();
    };
    if original.is_empty() {
        return Vec::new();
    }
    let toggled: String = original
        .chars()
        .flat_map(|character| {
            let swapped: Vec<char> = if character.is_uppercase() {
                character.to_lowercase().collect()
            } else {
                character.to_uppercase().collect()
            };
            swapped
        })
        .collect();
    if toggled.len() != original.len() {
        return Vec::new();
    }
    vec![replace(range, toggled), caret(cursor_after)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `|` marks the cursor in `before` and `after`.
    fn run(before: &str, keys: &str, after: &str) {
        let mut vim = Vim::default();
        let (text, _mode) = run_with(&mut vim, before, keys);
        assert_eq!(text, after, "keys {keys:?} on {before:?}");
    }

    fn run_with(vim: &mut Vim, before: &str, keys: &str) -> (String, Mode) {
        let cursor = before.find('|').expect("cursor marker");
        let mut text = before.replacen('|', "", 1);
        let mut cursor = cursor;
        for key in parse_keys(keys) {
            let outcome = vim.handle_key(
                key,
                Buffer {
                    text: &text,
                    cursor,
                },
            );
            if !outcome.handled {
                if let Key::Char(character) = key {
                    text.insert(cursor, character);
                    cursor += character.len_utf8();
                }
                continue;
            }
            assert!(
                !outcome.edits.contains(&Edit::Undo),
                "undo is applied by the editor"
            );
            (text, cursor) = apply_edits(&text, cursor, &outcome.edits);
        }
        let mut marked = text.clone();
        marked.insert(cursor.min(marked.len()), '|');
        (marked, vim.mode())
    }

    fn parse_keys(keys: &str) -> Vec<Key> {
        let mut parsed = Vec::new();
        let mut rest = keys;
        while !rest.is_empty() {
            let named = [
                ("<esc>", Key::Escape),
                ("<cr>", Key::Enter),
                ("<bs>", Key::Backspace),
                ("<c-r>", Key::Ctrl('r')),
                ("<c-d>", Key::Ctrl('d')),
            ];
            if let Some((name, key)) = named.iter().find(|(name, _)| rest.starts_with(name)) {
                parsed.push(*key);
                rest = &rest[name.len()..];
                continue;
            }
            let character = rest.chars().next().unwrap();
            parsed.push(Key::Char(character));
            rest = &rest[character.len_utf8()..];
        }
        parsed
    }

    #[test]
    fn moves_by_characters_and_lines() {
        run("|select", "ll", "se|lect");
        run("sel|ect", "h", "se|lect");
        run("|ab", "lll", "a|b");
        run("|abc\nde\nfghij", "llj", "abc\nd|e\nfghij");
        run("|abc\nde\nfghij", "llljj", "abc\nde\nfg|hij");
        run("ab|c\nd", "k", "ab|c\nd");
        run("abc\nd|e", "k", "a|bc\nde");
    }

    #[test]
    fn moves_by_words() {
        run("|select a.b from t", "w", "select |a.b from t");
        run("|select a.b from t", "ww", "select a|.b from t");
        run("|select a.b from t", "WW", "select a.b |from t");
        run("select a.b |from t", "b", "select a.|b from t");
        run("select a.b |from t", "B", "select |a.b from t");
        run("|select a.b", "e", "selec|t a.b");
        run("|select a.b", "ee", "select |a.b");
        run("|one\n\ntwo", "w", "one\n|\ntwo");
        run("|one two three", "2w", "one two |three");
    }

    #[test]
    fn moves_within_lines_and_files() {
        run("  select |x", "0", "|  select x");
        run("  select |x", "^", "  |select x");
        run("|select x\nfrom", "$", "select |x\nfrom");
        run("a\nb\n|c", "gg", "|a\nb\nc");
        run("|a\n  b\nc", "G", "a\n  b\n|c");
        run("|a\n  b\nc", "2G", "a\n  |b\nc");
        run("|select a, b, c", "f,", "select a|, b, c");
        run("|select a, b, c", "t,", "select |a, b, c");
        run("|select a, b, c", "f,;", "select a, b|, c");
        run("select a, b, |c", "F,", "select a, b|, c");
        run("|a\nb\n\nc\nd\n\ne", "}", "a\nb\n|\nc\nd\n\ne");
        run("|a\nb\n\nc\nd\n\ne", "}}", "a\nb\n\nc\nd\n|\ne");
        run("a\n\nb\n|c", "{", "a\n|\nb\nc");
    }

    #[test]
    fn deletes_with_motions() {
        run("|select a from t", "dw", "|a from t");
        run("select |a from t", "d2w", "select |t");
        run("select |a from t", "2dw", "select |t");
        run("select |a, b", "D", "select| ");
        run("select |a", "x", "select| ");
        run("sel|ect", "X", "se|ect");
        run("|select a,\nb", "dw", "|a,\nb");
        run("select |last\nnext", "dw", "select| \nnext");
        run("select |a, b, c", "dt,", "select |, b, c");
        run("select |a, b, c", "df,", "select | b, c");
        run("|abc def", "de", "| def");
    }

    #[test]
    fn deletes_lines() {
        run("one\n|two\nthree", "dd", "one\n|three");
        run("one\ntwo\n|three", "dd", "one\n|two");
        run("|one\ntwo\nthree", "2dd", "|three");
        run("|one\ntwo\nthree", "dj", "|three");
        run("one\n  |two\nthree", "dk", "|three");
        run("|only", "dd", "|");
        run("a\n|b\nc\nd", "dG", "|a");
    }

    #[test]
    fn changes_text_and_enters_insert_mode() {
        let mut vim = Vim::default();
        let (text, mode) = run_with(&mut vim, "select |name from t", "ciw");
        assert_eq!(text, "select | from t");
        assert_eq!(mode, Mode::Insert);
        run("select |name from t", "cwid<esc>", "select i|d from t");
        run("select |a, b", "C<esc>", "select| ");
        run("  |select x\nnext", "ccy<esc>", "  |y\nnext");
        run("|ab", "sX<esc>", "|Xb");
        run("count(|*)", "ci(1<esc>", "count(|1)");
        run("where name = '|bob'", "ci'x<esc>", "where name = '|x'");
        run("where |name = 'bob'", "ci'x<esc>", "where name = '|x'");
        run("f(a, (|b))", "da(", "f(a, |)");
    }

    #[test]
    fn word_text_objects() {
        run("select |name from t", "diw", "select | from t");
        run("select |name from t", "daw", "select |from t");
        run("select a |name", "daw", "select |a");
        run("x = |a.b.c", "diW", "x =| ");
    }

    #[test]
    fn inserts_and_appends() {
        run("sel|ect", "iX<esc>", "sel|Xect");
        run("sel|ect", "aX<esc>", "sele|Xct");
        run("  sel|ect", "IX<esc>", "  |Xselect");
        run("sel|ect", "AX<esc>", "select|X");
        run("  sel|ect\nnext", "onew<esc>", "  select\n  ne|w\nnext");
        run("  sel|ect", "Onew<esc>", "  ne|w\n  select");
    }

    #[test]
    fn yanks_and_pastes() {
        run("|one\ntwo", "yyp", "one\n|one\ntwo");
        run("|one\ntwo", "yyjp", "one\ntwo\n|one");
        run("one\n|two", "yyP", "one\n|two\ntwo");
        run("|ab cd", "ywP", "ab| ab cd");
        run("|ab cd", "yw$p", "ab cdab| ");
        run("|abc", "xp", "b|ac");
        run("one\n|two", "ddP", "|two\none");
    }

    #[test]
    fn undo_and_redo_are_left_to_the_editor() {
        let mut vim = Vim::default();
        let outcome = vim.handle_key(
            Key::Char('u'),
            Buffer {
                text: "a",
                cursor: 0,
            },
        );
        assert_eq!(outcome.edits, vec![Edit::Undo]);
        let outcome = vim.handle_key(
            Key::Ctrl('r'),
            Buffer {
                text: "a",
                cursor: 0,
            },
        );
        assert_eq!(outcome.edits, vec![Edit::Redo]);
    }

    #[test]
    fn repeats_last_change() {
        run("|a b c d", "dw..", "|d");
        run("|x\ny\nz", "A;<esc>j.j.", "x;\ny;\nz|;");
        run("select |a, b", "ciwz<esc>ww.", "select z, |z");
        run("|ab\ncd", "dd.", "|");
    }

    #[test]
    fn visual_mode_selects_and_operates() {
        run("select |abc def", "vlld", "select | def");
        run("select |abc def", "vey", "select |abc def");
        run("select |abc def", "veyP", "select ab|cabc def");
        run("one\n|two\nthree", "Vjd", "|one");
        run("one\n|two\nthree", "Vd", "one\n|three");
        run("f(|abc)", "vi(d", "f(|)");
        run("select |abc", "vlc", "select |c");
        run("select |abc", "vl<esc>", "select a|bc");
        run("select |abc", "vl~", "select |ABc");
    }

    #[test]
    fn searches_forward_and_backward() {
        run(
            "|from a join b join c",
            "/join<cr>",
            "from a |join b join c",
        );
        run(
            "|from a join b join c",
            "/join<cr>n",
            "from a join b |join c",
        );
        run(
            "from a join b |join c",
            "/join<cr>",
            "from a |join b join c",
        );
        run(
            "from a join b |join c",
            "/join<cr>N",
            "from a join b |join c",
        );
        run("|From x from", "/from<cr>", "From x |from");
        run("|From x from", "/From<cr>n", "|From x from");
        run("|name, x, name", "*", "name, x, |name");
    }

    #[test]
    fn misc_commands() {
        run("|a\n  b\nc", "J", "a| b\nc");
        run("|a\nb\nc", "3J", "a b| c");
        run("|abc", "rx", "|xbc");
        run("|abc", "2rx", "x|xc");
        run("|abc", "~~", "AB|c");
    }

    #[test]
    fn counts_multiply_and_zero_is_a_motion() {
        run("|a b c d e f g", "2d2w", "|e f g");
        run("ab|c", "0", "|abc");
        run(
            "|a b c d e f g h i j k l",
            "10w",
            "a b c d e f g h i j |k l",
        );
    }

    #[test]
    fn escape_and_invalid_keys_clear_pending_command() {
        run("|abc", "d<esc>x", "|bc");
        run("|abc", "dQx", "|bc");
        let mut vim = Vim::default();
        vim.handle_key(
            Key::Char('d'),
            Buffer {
                text: "a",
                cursor: 0,
            },
        );
        assert_eq!(vim.pending_text(), "d");
    }

    #[test]
    fn insert_mode_passes_keys_to_the_editor() {
        let mut vim = Vim::default();
        vim.handle_key(
            Key::Char('i'),
            Buffer {
                text: "",
                cursor: 0,
            },
        );
        let outcome = vim.handle_key(
            Key::Char('x'),
            Buffer {
                text: "",
                cursor: 0,
            },
        );
        assert!(!outcome.handled);
        let outcome = vim.handle_key(
            Key::Enter,
            Buffer {
                text: "x",
                cursor: 1,
            },
        );
        assert!(!outcome.handled);
    }

    #[test]
    fn app_shortcuts_pass_through_in_normal_mode() {
        let mut vim = Vim::default();
        let outcome = vim.handle_key(
            Key::Ctrl('t'),
            Buffer {
                text: "",
                cursor: 0,
            },
        );
        assert!(!outcome.handled);
        let outcome = vim.handle_key(
            Key::Char('q'),
            Buffer {
                text: "",
                cursor: 0,
            },
        );
        assert!(
            outcome.handled,
            "unbound printable keys never reach the text"
        );
    }

    #[test]
    fn ctrl_w_is_left_to_the_editor_only_in_insert_mode() {
        let mut vim = Vim::default();
        let normal = vim.handle_key(
            Key::Ctrl('w'),
            Buffer {
                text: "a b",
                cursor: 2,
            },
        );
        assert_eq!(normal, Outcome::handled(Vec::new()));
        vim.handle_key(
            Key::Char('i'),
            Buffer {
                text: "a b",
                cursor: 2,
            },
        );
        let insert = vim.handle_key(
            Key::Ctrl('w'),
            Buffer {
                text: "a b",
                cursor: 2,
            },
        );
        assert!(!insert.handled);
    }

    #[test]
    fn handles_multibyte_text() {
        run("|año ñu", "w", "año |ñu");
        run("|año ñu", "x", "|ño ñu");
        run("añ|o", "h", "a|ño");
        run("|ñandú", "$", "ñand|ú");
    }
}
