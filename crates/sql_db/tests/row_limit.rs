//! A lone read stops on the server at the row limit instead of streaming every row.
//!
//! Needs servers: `QUERENT_SQLSERVER=<connection string> QUERENT_PG_URL=postgres://... cargo test -p sql_db --test row_limit -- --ignored`

use std::time::{Duration, Instant};

use sql_db::{
    CancelHandle, CatalogObject, Connection, ConnectionSettings, ObjectKind, QueryOutput,
};

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
#[ignore = "needs a SQL Server; set QUERENT_SQLSERVER"]
async fn sql_server_stops_at_the_limit() {
    check(
        "QUERENT_SQLSERVER",
        "select top 30000000 a.object_id, b.name from sys.all_columns a cross join sys.all_columns b",
    )
    .await;
}

#[tokio::test]
#[ignore = "needs a SQL Server; set QUERENT_SQLSERVER"]
async fn sql_server_runs_go_batches_and_unseparated_statements() {
    let text = std::env::var("QUERENT_SQLSERVER").unwrap();
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

/// The CREATE script of a table built with keys, a default, a check and an
/// index runs on the server it came from.
#[expect(clippy::unwrap_used, reason = "a test helper")]
async fn check_create_script(
    variable: &str,
    database: &str,
    schema: &str,
    setup: &[&str],
    column_extras: usize,
) {
    let text = std::env::var(variable).unwrap();
    let settings = ConnectionSettings::from_connection_string(&text).unwrap();
    let dialect = settings.driver.dialect();
    let mut connection = Connection::connect(settings.config().unwrap())
        .await
        .unwrap();
    let cancel = CancelHandle::new();
    for sql in [
        "drop table if exists querent_child_test",
        "drop table if exists querent_parent_test",
    ]
    .iter()
    .chain(setup)
    {
        connection.execute(sql, &cancel).await.unwrap();
    }
    let table = CatalogObject {
        schema: schema.to_owned(),
        name: "querent_child_test".to_owned(),
        kind: ObjectKind::Table,
    };
    let columns = connection.list_columns(database, &table).await.unwrap();
    let details = connection.table_details(database, &table).await.unwrap();
    let script = sql_db::create_table_script(dialect, &table, &columns, &details);
    let kinds: Vec<_> = details.constraints.iter().map(|found| found.kind).collect();
    connection
        .execute("drop table querent_child_test", &cancel)
        .await
        .unwrap();
    let rebuilt = connection.execute(&script, &cancel).await;
    let again = connection.table_details(database, &table).await;
    for sql in [
        "drop table if exists querent_child_test",
        "drop table if exists querent_parent_test",
    ] {
        connection.execute(sql, &cancel).await.unwrap();
    }
    rebuilt.unwrap_or_else(|error| panic!("{error}\n{script}"));
    assert_eq!(again.unwrap(), details, "{script}");
    assert_eq!(
        kinds,
        [
            sql_db::ConstraintKind::PrimaryKey,
            sql_db::ConstraintKind::Unique,
            sql_db::ConstraintKind::ForeignKey,
            sql_db::ConstraintKind::Check,
        ],
        "{script}"
    );
    assert_eq!(details.indexes.len(), 1, "{script}");
    assert_eq!(details.column_extras.len(), column_extras, "{script}");
}

#[tokio::test]
#[ignore = "needs a SQL Server; set QUERENT_SQLSERVER"]
async fn sql_server_scripts_a_table_that_runs() {
    check_create_script(
        "QUERENT_SQLSERVER",
        "master",
        "dbo",
        &[
            "create table querent_parent_test (id int not null primary key)",
            "create table querent_child_test (
                a int identity(5, 2) not null,
                b int not null constraint querent_df default (7),
                c nvarchar(20) null,
                d as (b * 2),
                constraint querent_pk primary key (b, a),
                constraint querent_uq unique (c),
                constraint querent_fk foreign key (b) references querent_parent_test (id) on delete cascade,
                constraint querent_ck check (b > 0))",
            "create index querent_ix on querent_child_test (c desc) include (b) where c is not null",
        ],
        3,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL server; set QUERENT_PG_URL"]
async fn postgres_scripts_a_table_that_runs() {
    check_create_script(
        "QUERENT_PG_URL",
        "postgres",
        "public",
        &[
            "create table querent_parent_test (id int not null primary key)",
            "create table querent_child_test (
                a serial,
                b int not null default 7,
                c varchar(20),
                constraint querent_pk primary key (b, a) include (c),
                constraint querent_uq unique (c),
                constraint querent_fk foreign key (b) references querent_parent_test (id) on delete cascade,
                constraint querent_ck check (b > 0))",
            "create index querent_ix on querent_child_test (c desc) include (b) where c is not null",
        ],
        2,
    )
    .await;
}

/// A run reports whether it left a transaction open.
#[expect(clippy::unwrap_used, reason = "a test helper")]
async fn check_open_transaction(variable: &str, begin: &str) {
    let text = std::env::var(variable).unwrap();
    let config = ConnectionSettings::from_connection_string(&text)
        .unwrap()
        .config()
        .unwrap();
    let mut connection = Connection::connect(config).await.unwrap();
    for (sql, open) in [("select 1", false), (begin, true), ("rollback", false)] {
        let output = connection.execute(sql, &CancelHandle::new()).await.unwrap();
        assert_eq!(output.open_transaction, Some(open), "{sql}");
    }
}

#[tokio::test]
#[ignore = "needs a SQL Server; set QUERENT_SQLSERVER"]
async fn sql_server_reports_an_open_transaction() {
    check_open_transaction("QUERENT_SQLSERVER", "begin transaction; select 1").await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL server; set QUERENT_PG_URL"]
async fn postgres_reports_an_open_transaction() {
    check_open_transaction("QUERENT_PG_URL", "begin; select 1").await;
}

/// The error names the line of the sent text, counted from its first line.
#[expect(clippy::unwrap_used, reason = "a test helper")]
async fn check_error_line(variable: &str, sql: &str, line: usize) {
    let text = std::env::var(variable).unwrap();
    let config = ConnectionSettings::from_connection_string(&text)
        .unwrap()
        .config()
        .unwrap();
    let mut connection = Connection::connect(config).await.unwrap();
    let error = connection
        .execute(sql, &CancelHandle::new())
        .await
        .unwrap_err();
    assert_eq!(error.line(), Some(line), "{error}");
}

#[tokio::test]
#[ignore = "needs a SQL Server; set QUERENT_SQLSERVER"]
async fn sql_server_errors_name_their_line() {
    let missing = "\n\nselect 1\nfrom querent_no_such_table";
    check_error_line("QUERENT_SQLSERVER", missing, 3).await;
    let second_batch = "select 1\nGO\n\nselect *\nfrom querent_no_such_table";
    check_error_line("QUERENT_SQLSERVER", second_batch, 4).await;
    // An error raised inside a procedure names a line of the procedure, so none is reported.
    let text = std::env::var("QUERENT_SQLSERVER").unwrap();
    let config = ConnectionSettings::from_connection_string(&text)
        .unwrap()
        .config()
        .unwrap();
    let mut connection = Connection::connect(config).await.unwrap();
    let in_procedure = "\n\n\nexec sp_executesql N'select * from querent_no_such_table'";
    let error = connection
        .execute(in_procedure, &CancelHandle::new())
        .await
        .unwrap_err();
    assert!(matches!(error.line(), None | Some(4)), "{error:?}");
}

#[tokio::test]
#[ignore = "needs a PostgreSQL server; set QUERENT_PG_URL"]
async fn postgres_errors_name_their_line() {
    let missing = "select 1;\nselect *\nfrom querent_no_such_table";
    check_error_line("QUERENT_PG_URL", missing, 3).await;
}

#[tokio::test]
#[ignore = "needs a SQL Server; set QUERENT_SQLSERVER"]
async fn sql_server_keeps_results_from_before_an_error() {
    check_partial_output("QUERENT_SQLSERVER").await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL server; set QUERENT_PG_URL"]
async fn postgres_keeps_results_from_before_an_error() {
    check_partial_output("QUERENT_PG_URL").await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL server; set QUERENT_PG_URL"]
async fn postgres_stops_at_the_limit() {
    check(
        "QUERENT_PG_URL",
        "select g, md5(g::text) from generate_series(1, 20000000) g",
    )
    .await;
}
