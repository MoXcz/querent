//! A lone read stops on the server at the row limit instead of streaming every row.
//!
//! Needs servers: `SQL_EDITOR_SQLSERVER=<connection string> SQL_EDITOR_PG_URL=postgres://... cargo test -p sql_db --test row_limit -- --ignored`

use std::time::{Duration, Instant};

use sql_db::{CancelHandle, Connection, ConnectionSettings};

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
#[ignore = "needs a PostgreSQL server; set SQL_EDITOR_PG_URL"]
async fn postgres_stops_at_the_limit() {
    check(
        "SQL_EDITOR_PG_URL",
        "select g, md5(g::text) from generate_series(1, 20000000) g",
    )
    .await;
}
