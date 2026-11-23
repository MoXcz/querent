# dbt projects

Open a folder that holds a `dbt_project.yml`, or a repository with one up to four folders down, and a **dbt** view appears in the sidebar next to Files and Database. It reads what dbt writes in `target/`, so it needs no dbt install and never changes the project. Run `dbt parse` or `dbt compile` once to create `target/manifest.json`; the view reloads when dbt writes it again.

```text
cima
├── Models (140)
│   └── marts/
│       └── finanzas/
│           └── fact_estado_resultados    table · finanzas
├── Sources (71)
├── Seeds, Snapshots, Tests, Analyses, Macros, Exposures
```

| To | Do |
| --- | --- |
| Find a model | type in the filter: part of a name, `tag:finanzas` or `group:ventas` |
| See what a model reads and what reads it | expand it: `↑` rows are upstream, `↓` downstream; click one to jump to it |
| Open its file | double-click it, or right-click > Open File |
| See its SQL with refs resolved | right-click > Open Compiled SQL |
| Run it | right-click > Preview: runs the compiled SQL under the row limit |
| Query the built table or view | right-click > Select Top 1000 Rows, or Show in Database |
| Query a dev database instead | right-click the project > Preview Database…, for example `analytics_dev` |

Queries run on the connection you last ran a file from the project's folder on, or else the current tab's.

Open Compiled SQL and Preview use `target/compiled/` when it has the model. Without it, `ref()` and `source()` are filled in from the manifest; a model that uses other Jinja (macros, `if` blocks) asks you to compile the project first. A model edited after dbt last wrote the manifest is marked **changed**: its compiled SQL may be old.

Inside `{{ ref('` and `{{ source('` in the editor, completion offers the project's models, sources and source tables.
