# Editor

## Explorer

Click a connection to use it in the current tab and expand its databases (warehouses and lakehouses on Fabric). The tab's database picker lists the same databases; type to filter it, or type a name that is not listed and pick "Use …".

Expand a database to browse its tables, views, procedures and functions.

- Right-click for Select Top 1000 Rows, Script as SELECT or CREATE, and Copy name.
- Double-click to insert a name at the cursor.

On SQL Server and PostgreSQL an expanded table lists its keys, checks and indexes under its columns. Its CREATE script includes them, with defaults, identity and computed columns. The script leaves out a default constraint's own name, triggers, partitioning, and XML, spatial and hash indexes; a PostgreSQL `serial` column is written as an identity column.

## Results

Above the grid sit a button per result set, Messages, and Export CSV and JSON.

| To | Do |
| --- | --- |
| Keep only matching rows | type in "Filter rows" under the grid |
| Sort | click a column's sort arrow (NULL first, numbers by value) |
| Copy a row, with or without column names, or every row | right-click a row |
| See a value longer than 500 characters | right-click a row and view its full values; each has a Copy button |

The exports and "Copy all rows" write the rows the grid shows, in its order. Copied rows are tab-separated, so they paste into a spreadsheet.

### Errors

When the server reports where a statement failed, the error starts with the tab's line number and a button jumps to that line. SQL Server and PostgreSQL report a line for most errors; SQLite and Snowflake only for syntax errors.

### Open transactions

The status line starts with "Open transaction" when a run on SQL Server, PostgreSQL or SQLite left one open on the tab's connection. Closing that tab or the window then asks first, because the server rolls the transaction back.

### Row limit

With a row limit set in Settings, a single read-only statement stops on the server once the limit is reached. These still read to the end and keep only the first rows, so nothing they write is cut short:

- a script with several statements
- a statement calling a function that is not a common built-in, since it might write
- a run inside an open transaction

## Completion

While you type, a menu offers tables after `from` and `join`, the columns of `alias.`, and keywords. Hover over a table or column to see its columns or type.

Tables and columns are read once per database in the background; "Refresh schema" in the command palette reads them again. PostgreSQL, SQLite and Snowflake tabs also underline syntax errors.

A language server can add its own completions; see [Settings](settings.md#language-servers).

## Files

A tab can hold a `.sql` file. Open one with `Ctrl+O` or by dropping it on the window. The tab title is the file name, with `*` while it has unsaved changes; closing it then asks first.

| Encoding read | When |
| --- | --- |
| UTF-8 | by default |
| UTF-16 | the file starts with a byte order mark, as SQL Server Management Studio writes them |
| Windows-1252 | the file is not valid UTF-8 |

Files are always saved as UTF-8.

When another program changes a tab's file, the tab takes the new text the next time you switch to it or back to the app. A tab with changes of its own keeps them and warns that saving will overwrite the file.

## Session and history

Tabs and their text come back after a restart, from `~/.config/querent/session.json`. Every run is kept in `history.jsonl` next to it, except statements that look like they set a password; the tab's own text is still saved with the session.

Both files are readable only by you, since queries can hold sensitive values. "Clear query history" in the command palette empties the history.

## Without the UI

```sh
cargo run -p sql_db --example query -- "<connection string>" "select top 10 * from sys.tables"
```
