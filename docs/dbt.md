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
────────────────────────────────────────────
fact_estado_resultados · model     Back  Direct  Show in tree
Reads from (3)
    stg_sap__contracts             model
    erp.gl_entries                 source
Read by (1)
    rpt_finanzas                   model
```

Under the tree, the lineage panel shows what the selected model or source reads and what reads it.

| To | Do |
| --- | --- |
| Find a model | type in the filter: part of a name, `tag:finanzas` or `group:ventas` |
| See what a model reads and what reads it | select it; the panel under the tree lists both |
| Follow the lineage further | click an entry in the panel to show its lineage there; the tree does not move. Back returns to the one before |
| See every step, not only direct reads | click Direct in the panel to switch to All steps; each entry says how many steps away it is |
| Find a lineage entry in the tree | double-click it, or Show in tree: opens only the folders that lead to it |
| Open its file | double-click it, or right-click > Open File |
| See its SQL with refs resolved | right-click > Open Compiled SQL |
| Run it | right-click > Preview: runs the compiled SQL under the row limit |
| Query the built table or view | right-click > Select Top 1000 Rows, or Show in Database |
| Query a dev database instead | right-click the project > Preview Database…, for example `analytics_dev` |

Queries run on the connection you last ran a file from the project's folder on, or else the current tab's.

Open Compiled SQL and Preview use `target/compiled/` when it has the model. Without it, `ref()` and `source()` are filled in from the manifest; a model that uses other Jinja (macros, `if` blocks) asks you to compile the project first. A model edited after dbt last wrote the manifest is marked **changed**: its compiled SQL may be old.

Inside `{{ ref('` and `{{ source('` in the editor, completion offers the project's models, sources and source tables.
