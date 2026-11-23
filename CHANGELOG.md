# Changelog

Querent shows a version's section once, at the first start after an update, and under "What's new" in the command palette. The release job copies the same section into the GitHub release, and fails when the tag has no section here.

## 0.4.1

- `Ctrl+Enter` now runs up to the next `;` or `GO` line, not the next blank line. A `delete from t` followed by a blank line and `where ...` runs with its `where`, and dbt models with blank lines run whole. Without any `;` in the tab it runs the whole tab; select a query to run only it.
- To end statements at blank lines again, set "Ctrl+Enter runs up to" in Settings to "Also at a blank line". Even then, a blank line inside parentheses, after a comma or a word such as `from`, or in a `with` before its main `select` no longer cuts the statement.

## 0.4.0

### dbt projects

- A folder with a dbt project (`dbt_project.yml`, up to four folders down) adds a **dbt** view to the sidebar, read from `target/manifest.json`. No dbt install is needed.
- Models, sources, seeds, snapshots, tests, analyses, macros and exposures, grouped by folder, with each model's materialization and tags. Filter by name, `tag:x` or `group:x`.
- Expand a model to see what it reads and what reads it; click one to jump there.
- Right-click a model to open its file or its compiled SQL, preview its rows, select from its table or view, or show it in the Database view. Preview Database on the project switches previews to another database, such as `analytics_dev`.
- Models edited since dbt last ran are marked **changed**.
- Completion inside `ref('` and `source('` offers the project's models and sources.

### Fixes

- Vim normal mode shows a solid block cursor, with the character under it in the background color, and the block shows on empty lines too.

## 0.3.1

- Select a block of cells in the results grid: click a cell, then drag or Shift-click to another. A border marks the selection.
- `Ctrl+C` copies the selected cells and `Ctrl+Shift+C` adds a line of their column names (Cmd on macOS). They paste into Excel or Google Sheets one value per cell. Right-click also offers both.
- Shift-click and Ctrl-click no longer select whole rows; select across every column instead, or right-click a row to copy it.

## 0.3.0

### Folders

- **Open Folder** (`ctrl-shift-o`) shows a folder of `.sql` files in the sidebar, under the new Files view. Dropping a folder on the window opens it too. Open folders come back after a restart.
- The tree hides dotfiles and whatever `.gitignore` lists. "All files" shows them.
- Click a file to open it. The tree follows files added, renamed or deleted in other programs, and an open tab follows a rename.
- The filter box finds files by name or path and opens the folders that hold them.
- Right-click for New File, New Folder, Rename, Delete (to the trash), Open and Run, Copy Path and Reveal in File Manager.
- A file opened from a folder starts on the connection you last ran one of that folder's files on.
- **Find in Folder** (`ctrl-alt-f`) searches the text of every file, as plain text or a regular expression. Click a line to open the file there.

## 0.2.2

- **Check for updates** in the command palette, and once at start, looks for a newer release on GitHub. Querent installs it when you agree, after checking its signature.
- Completion offers tables and columns in other databases (`other_db.schema.table`).
- Copy, cut and paste with nothing selected act on the whole line.
