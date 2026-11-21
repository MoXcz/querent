//! Needs a server: `QUERENT_PG_URL=postgres://user:password@host:port/db cargo test -p sql_db --test postgres -- --ignored`

use std::time::{Duration, Instant};

use sql_db::{CancelHandle, Connection, ConnectionSettings, QueryError};

#[tokio::test]
#[ignore = "needs a PostgreSQL server"]
async fn cancel_stops_a_running_query_and_keeps_the_connection() {
    let url = std::env::var("QUERENT_PG_URL").expect("set QUERENT_PG_URL");
    let config = ConnectionSettings::from_connection_string(&url)
        .unwrap()
        .config()
        .unwrap();
    let mut connection = Connection::connect(config).await.unwrap();
    let cancel = CancelHandle::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        canceller.cancel();
    });
    let started = Instant::now();
    let result = connection.execute("select pg_sleep(30)", &cancel).await;
    assert!(matches!(result, Err(QueryError::Cancelled)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    let output = connection
        .execute("select 1", &CancelHandle::new())
        .await
        .unwrap();
    assert_eq!(output.result_sets.len(), 1);
}
