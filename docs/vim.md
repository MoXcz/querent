# Vim

Vim keys are on by default in the SQL editor, and the status bar shows the mode. Turn them off in Settings (`Ctrl+,`) or with "Vim mode" in the command palette. App shortcuts such as `Ctrl+Enter` work in every mode.

| Kind | Supported |
| --- | --- |
| Modes | normal, insert, visual, visual line |
| Motions | `h j k l w W b B e E 0 ^ $ gg G {count}G f F t T ; , { } n N ctrl-d ctrl-u`, Enter, Space, Backspace |
| Operators | `d c y` with motions and counts (`2d3w`); `dd cc yy` |
| Text objects | `iw aw iW aW i( a( ib i[ i{ iB i< i' i"` and ``i` `` |
| Edits | `x X D C s S Y p P J ~ r` |
| Insert | `i a I A o O` |
| Other | `u`, `ctrl-r`, `.`, `/` search with smartcase, `*` |

`Ctrl+W` deletes the word before the cursor in insert mode.
