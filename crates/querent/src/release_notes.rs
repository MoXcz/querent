//! What changed in each version, from `CHANGELOG.md`, shown once after an update.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::text::TextView;
use gpui_kit::{App, ParentElement as _, Styled as _, Window, actions, div, px};
use semver::Version;

actions!(
    querent,
    [
        /// Show what changed in this version of Querent.
        ShowReleaseNotes
    ]
);

const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Each `## <version>` section of the changelog, in file order, with its heading.
fn sections(changelog: &str) -> Vec<(Version, &str)> {
    let mut starts: Vec<(usize, Version)> = Vec::new();
    let mut offset = 0;
    for line in changelog.split_inclusive('\n') {
        if let Some(heading) = line.strip_prefix("## ")
            && let Some(Ok(version)) = heading.split_whitespace().next().map(Version::parse)
        {
            starts.push((offset, version));
        }
        offset += line.len();
    }
    let ends = starts
        .iter()
        .skip(1)
        .map(|(start, _)| *start)
        .chain([changelog.len()]);
    starts
        .iter()
        .zip(ends)
        .filter_map(|((start, version), end)| {
            Some((version.clone(), changelog.get(*start..end)?.trim_end()))
        })
        .collect()
}

/// The sections newer than `seen` up to `current`, newest first, as one Markdown text.
fn notes_between(changelog: &str, seen: Option<&Version>, current: &Version) -> String {
    let mut shown: Vec<(Version, &str)> = sections(changelog)
        .into_iter()
        .filter(|(version, _)| version <= current && seen.is_none_or(|seen| version > seen))
        .collect();
    shown.sort_by(|left, right| right.0.cmp(&left.0));
    shown
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// What the last session says about the version that wrote it.
#[derive(Debug, Clone, Copy)]
pub enum LastStart<'a> {
    /// No session: a new install has nothing to announce.
    None,
    /// A session from before versions were recorded.
    Unrecorded,
    Version(&'a str),
}

/// Notes to show at start, or `None` on a first start or when nothing is new.
pub fn notes_after_update(last: LastStart<'_>) -> Option<String> {
    let current = Version::parse(CURRENT_VERSION).ok()?;
    let seen = match last {
        LastStart::None => return None,
        // Any older version could have written it; show only this one's notes.
        LastStart::Unrecorded => return current_section(CHANGELOG, &current),
        LastStart::Version(text) => Version::parse(text).ok(),
    };
    if seen.as_ref().is_some_and(|seen| *seen >= current) {
        return None;
    }
    let notes = notes_between(CHANGELOG, seen.as_ref(), &current);
    (!notes.is_empty()).then_some(notes)
}

fn current_section(changelog: &str, current: &Version) -> Option<String> {
    sections(changelog)
        .into_iter()
        .find(|(version, _)| version == current)
        .map(|(_, text)| text.to_owned())
}

/// Opens the dialog with `notes`, or this version's section when `None`.
pub fn show(notes: Option<String>, window: &mut Window, cx: &mut App) {
    let notes = notes.or_else(|| {
        let current = Version::parse(CURRENT_VERSION).ok()?;
        current_section(CHANGELOG, &current)
    });
    let Some(notes) = notes else {
        window.push_notification(
            format!("No release notes for Querent {CURRENT_VERSION}."),
            cx,
        );
        return;
    };
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title(format!("What's new in Querent {CURRENT_VERSION}"))
            .w(px(640.))
            .child(
                div()
                    .h(px(440.))
                    .child(TextView::markdown("release-notes", notes.clone()).scrollable(true)),
            )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "# Changelog\n\nIntro.\n\n## 0.3.0\n\n- folders\n\n## 0.2.2\n\n- updates\n\n## 0.2.1\n\n- old\n";

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn shows_the_versions_after_the_last_seen_one_newest_first() {
        let notes = notes_between(LOG, Some(&version("0.2.1")), &version("0.3.0"));
        assert_eq!(notes, "## 0.3.0\n\n- folders\n\n## 0.2.2\n\n- updates");
        assert_eq!(
            notes_between(LOG, Some(&version("0.2.2")), &version("0.2.2")),
            ""
        );
        assert_eq!(
            notes_between(LOG, None, &version("0.2.2")),
            "## 0.2.2\n\n- updates\n\n## 0.2.1\n\n- old"
        );
    }

    #[test]
    fn an_unrecorded_session_shows_only_this_version() {
        assert_eq!(
            current_section(LOG, &version("0.2.2")).as_deref(),
            Some("## 0.2.2\n\n- updates")
        );
        let notes = notes_after_update(LastStart::Unrecorded).unwrap();
        assert!(notes.starts_with(&format!("## {CURRENT_VERSION}")));
        assert_eq!(notes.matches("\n## ").count(), 0);
    }

    #[test]
    fn first_start_and_same_version_show_nothing() {
        assert_eq!(notes_after_update(LastStart::None), None);
        assert_eq!(
            notes_after_update(LastStart::Version(CURRENT_VERSION)),
            None
        );
    }

    /// Catches a version bump without notes before the release job does.
    #[test]
    fn the_changelog_has_this_version() {
        let current = version(CURRENT_VERSION);
        assert!(
            sections(CHANGELOG)
                .iter()
                .any(|(found, _)| *found == current)
        );
    }
}
