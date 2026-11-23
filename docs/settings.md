# Settings

Open Settings with `Ctrl+,` or the gear in the status bar, or find any setting by name in the command palette (`Ctrl+Shift+P`). Settings cover the theme, vim mode, explorer grouping, format style and row limit, and are saved in `~/.config/querent/settings.toml`.

## Themes

Built in: Solitude (the default), Catppuccin (Latte, Frappé, Macchiato, Mocha) and Solarized (Dark, Light). Pick one in Settings or type "Theme" in the command palette.

To add a theme, drop a [gpui-kit theme file](https://github.com/longbridge/gpui-kit/tree/main/themes) into `~/.config/querent/themes/`. It loads without a restart, and a theme with the same name as a built-in one replaces it.

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
