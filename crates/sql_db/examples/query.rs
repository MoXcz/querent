//! Runs one batch against SQL Server or a Fabric SQL endpoint.
//!
//! `cargo run -p sql_db --example query -- "<connection string>" "<sql>"`

use std::io::Write as _;

use sql_db::{CancelHandle, TdsConnection};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let mut arguments = std::env::args().skip(1);
    let (Some(connection_string), Some(sql)) = (arguments.next(), arguments.next()) else {
        anyhow::bail!("usage: query <connection string> <sql>");
    };
    let config = sql_db::parse_connection_string_with_database(&connection_string)?;
    let started = std::time::Instant::now();
    let mut connection = TdsConnection::connect(config).await?;
    let connected = started.elapsed();
    let output = connection.execute(&sql, &CancelHandle::new()).await?;
    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "connected in {connected:?}, total {:?}",
        started.elapsed()
    )?;
    for result_set in &output.result_sets {
        let header: Vec<_> = result_set
            .columns
            .iter()
            .map(|column| format!("{} ({})", column.name, column.type_name))
            .collect();
        writeln!(stdout, "{}", header.join(" | "))?;
        for row in &result_set.rows {
            let cells: Vec<_> = row.iter().map(ToString::to_string).collect();
            writeln!(stdout, "{}", cells.join(" | "))?;
        }
        writeln!(
            stdout,
            "{} rows, truncated: {}",
            result_set.rows.len(),
            result_set.truncated
        )?;
    }
    writeln!(
        stdout,
        "rows affected: {:?}, messages: {:?}",
        output.rows_affected, output.messages
    )?;
    connection.close().await?;
    Ok(())
}
