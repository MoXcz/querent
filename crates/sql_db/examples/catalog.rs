//! Prints databases, the objects of one database, and the first table's columns.
//!
//! `cargo run -p sql_db --example catalog -- "<connection string>" <database>`

use std::io::Write as _;

use sql_db::{ObjectKind, TdsConnection};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let (Some(connection_string), Some(database)) = (arguments.next(), arguments.next()) else {
        anyhow::bail!("usage: catalog <connection string> <database>");
    };
    let config = sql_db::parse_connection_string(&connection_string)?;
    let mut connection = TdsConnection::connect_fabric(config).await?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "databases: {:?}", connection.list_databases().await?)?;
    let objects = connection.list_objects(&database).await?;
    for kind in ObjectKind::ALL {
        let count = objects.iter().filter(|object| object.kind == kind).count();
        writeln!(stdout, "{}: {count}", kind.plural_label())?;
    }
    if let Some(table) = objects.iter().find(|object| object.kind == ObjectKind::Table) {
        let columns = connection.list_columns(&database, table).await?;
        write!(stdout, "{}", sql_db::create_table_script(table, &columns))?;
    }
    if let Some(view) = objects.iter().find(|object| object.kind == ObjectKind::View) {
        let definition = connection.object_definition(&database, view).await?;
        let preview: String = definition.unwrap_or_default().chars().take(120).collect();
        writeln!(stdout, "{}: {preview}", view.qualified_name())?;
    }
    connection.close().await?;
    Ok(())
}
