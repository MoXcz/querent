//! A lone read stops on the server at the row limit instead of streaming every row.
//!
//! Needs servers: `SQL_EDITOR_SQLSERVER=<connection string> SQL_EDITOR_PG_URL=postgres://... cargo test -p sql_db --test row_limit -- --ignored`

use std::time::{Duration, Instant};

use sql_db::{CancelHandle, Connection, ConnectionSettings, QueryOutput};

#[expect(clippy::unwrap_used, reason = "a test helper")]
async fn check(variable: &str, big_query: &str) {
    let text = std::env::var(variable).unwrap();
    let config = ConnectionSettings::from_connection_string(&text)
        .unwrap()
        .config()
        .unwrap();
    let mut connection = Connection::connect(config).await.unwrap();
    connection.set_max_rows(1_000);
    let started = Instant::now();
    let output = connection
        .execute(big_query, &CancelHandle::new())
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(output.result_sets.len(), 1);
    assert_eq!(output.result_sets[0].rows.len(), 1_000);
    assert!(output.result_sets[0].truncated);
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    // The connection is ready for the next run.
    let next = connection
        .execute("select 1", &CancelHandle::new())
        .await
        .unwrap();
    assert_eq!(next.result_sets[0].rows.len(), 1);
}

#[tokio::test]
#[ignore = "needs a SQL Server; set SQL_EDITOR_SQLSERVER"]
async fn sql_server_stops_at_the_limit() {
    check(
        "SQL_EDITOR_SQLSERVER",
        "select top 30000000 a.object_id, b.name from sys.all_columns a cross join sys.all_columns b",
    )
    .await;
}

#[tokio::test]
#[ignore = "needs a SQL Server; set SQL_EDITOR_SQLSERVER"]
async fn sql_server_runs_go_batches_and_unseparated_statements() {
    let text = std::env::var("SQL_EDITOR_SQLSERVER").unwrap();
    let config = ConnectionSettings::from_connection_string(&text)
        .unwrap()
        .config()
        .unwrap();
    let mut connection = Connection::connect(config).await.unwrap();
    connection.set_max_rows(10);
    let script = "select 1\nGO\nselect 2\ngo\n";
    let output = connection
        .execute(script, &CancelHandle::new())
        .await
        .unwrap();
    assert_eq!(output.result_sets.len(), 2);
    // No `;` between them: the stop at the limit must not drop the second statement.
    let two = "select top 1000 a.object_id from sys.all_columns a select 2";
    let output = connection.execute(two, &CancelHandle::new()).await.unwrap();
    assert_eq!(output.result_sets.len(), 2);
    assert!(output.result_sets[0].truncated);
}

/// What arrived before a failing statement stays in the output.
#[expect(clippy::unwrap_used, reason = "a test helper")]
async fn check_partial_output(variable: &str) {
    let text = std::env::var(variable).unwrap();
    let config = ConnectionSettings::from_connection_string(&text)
        .unwrap()
        .config()
        .unwrap();
    let mut connection = Connection::connect(config).await.unwrap();
    let mut output = QueryOutput::default();
    let result = connection
        .execute_into("select 7; select 1/0;", &CancelHandle::new(), &mut output)
        .await;
    assert!(result.is_err());
    assert_eq!(output.result_sets.len(), 1);
    assert_eq!(output.result_sets[0].rows.len(), 1);
    let next = connection
        .execute("select 1", &CancelHandle::new())
        .await
        .unwrap();
    assert_eq!(next.result_sets.len(), 1);
}

#[tokio::test]
#[ignore = "needs a SQL Server; set SQL_EDITOR_SQLSERVER"]
async fn sql_server_keeps_results_from_before_an_error() {
    check_partial_output("SQL_EDITOR_SQLSERVER").await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL server; set SQL_EDITOR_PG_URL"]
async fn postgres_keeps_results_from_before_an_error() {
    check_partial_output("SQL_EDITOR_PG_URL").await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL server; set SQL_EDITOR_PG_URL"]
async fn postgres_stops_at_the_limit() {
    check(
        "SQL_EDITOR_PG_URL",
        "select g, md5(g::text) from generate_series(1, 20000000) g",
    )
    .await;
}
