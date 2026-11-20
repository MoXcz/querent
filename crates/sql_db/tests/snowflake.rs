//! Needs an account: `SQL_EDITOR_SNOWFLAKE="snowflake://user:password@account/DB/SCHEMA?warehouse=WH" cargo test -p sql_db --test snowflake -- --ignored`

use std::time::{Duration, Instant};

use sql_db::{CancelHandle, Connection, ConnectionSettings, QueryError, Value};

#[tokio::test]
#[ignore = "needs a Snowflake account"]
async fn runs_a_batch_and_cancels_a_running_query() {
    let text = std::env::var("SQL_EDITOR_SNOWFLAKE").expect("set SQL_EDITOR_SNOWFLAKE");
    let config = ConnectionSettings::from_connection_string(&text)
        .unwrap()
        .config()
        .unwrap();
    let mut connection = Connection::connect(config).await.unwrap();

    let output = connection
        .execute(
            "select 1 as n, 'a' as s, 1.50::number(5,2) as d, current_date() as t;\nselect 2;",
            &CancelHandle::new(),
        )
        .await
        .unwrap();
    assert_eq!(output.result_sets.len(), 2);
    assert_eq!(output.result_sets[0].rows[0][0], Value::Int(1));

    let cancel = CancelHandle::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        canceller.cancel();
    });
    let started = Instant::now();
    let result = connection.execute("call system$wait(60)", &cancel).await;
    assert!(matches!(result, Err(QueryError::Cancelled)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(20));
}
