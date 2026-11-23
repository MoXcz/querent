# Settings

Open Settings with `Ctrl+,` or the gear in the status bar, or find any setting by name in the command palette (`Ctrl+Shift+P`). Settings cover the theme, vim mode, explorer grouping, format style and row limit, and are saved in `~/.config/querent/settings.toml`.

## Themes

Built in: Solitude (the default), Catppuccin (Latte, Frappé, Macchiato, Mocha) and Solarized (Dark, Light). Pick one in Settings or type "Theme" in the command palette.

To add a theme, drop a [gpui-kit theme file](https://github.com/longbridge/gpui-kit/tree/main/themes) into `~/.config/querent/themes/`. It loads without a restart, and a theme with the same name as a built-in one replaces it. Its `selection.background` colors selected text, selected grid cells and the selected sidebar row. Write it with an alpha, such as `#6991b766` (40%), to set how strong it is; without one, gpui-kit shows it at 30% opacity at most.

## Running statements

`Ctrl+Enter` runs the selection, or else the statement under the cursor. "Ctrl+Enter runs up to" in Settings (`run_statement_ends_at` in `settings.toml`) sets where that statement ends:

| Value | A statement ends at | Use it when |
| --- | --- | --- |
| `semicolon` (default) | `;` or a line holding only `GO` | statements end in `;`; a `delete from t` can never run without the `where` on a later line |
| `blank_line` | also a blank line after a complete statement | a tab holds several queries without `;` |

With `blank_line`, a blank line inside parentheses, after a comma or a word such as `from`, `where` or `as`, or in a `with` before its main `select` does not end the statement, so dbt-formatted models run whole. Without any `;` in the tab, `semicolon` runs the whole tab; select a query to run only it.

## Updates

At startup Querent asks GitHub for the latest release; turn this off with "Check for updates" in Settings, or run "Check for updates" from the command palette at any time. A newer release shows a notification. Click it for the release notes, or click **Update** to download the build for your system, check its minisign signature against the key built into the app, and replace the program. The new version runs after a restart.

A copy installed with `cargo install`, a development build, or a copy in a folder Querent cannot write to (such as `/usr/bin`) only gets the notification.

## Language servers

A language server adds its completions, hover and diagnostics to the built-in ones. It can also format SQL instead of the built-in formatter ("Use the language server" in Settings).

Choose one per database kind (`sql_server`, `fabric`, `postgres`, `sqlite`, `snowflake`) in `settings.toml`:

```toml
[language_servers.postgres]
command = "sqls"
pass_connection = true   # send the connection, including its password, to the server
```

The status line shows the server's state; a server that stops restarts on next use.

## Files

```text
~/.config/querent/
  settings.toml      # settings and language servers
  connections.toml   # connection fields, no secrets
  secrets.toml       # secrets, only when there is no OS keychain
  session.json       # open tabs and their text
  history.jsonl      # every run
  themes/            # extra theme files
```
