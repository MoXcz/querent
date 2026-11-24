//! Reads a running REST catalog. Set `ICEBERG_REST_URI` (such as `http://localhost:8181`)
//! and run with `--ignored`; the catalog needs at least one table with a snapshot.

use sql_iceberg::{Catalog, CatalogConfig};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a running Iceberg REST catalog"]
async fn follows_a_table_down_to_its_files() {
    let Ok(uri) = std::env::var("ICEBERG_REST_URI") else {
        panic!("set ICEBERG_REST_URI");
    };
    let config = CatalogConfig {
        uri,
        warehouse: std::env::var("ICEBERG_WAREHOUSE").ok(),
        credential: None,
    };
    let catalog = Catalog::connect(&config).await.unwrap();
    let mut found = None;
    for namespace in catalog.namespaces(&[]).await.unwrap() {
        if let Some(table) = catalog.tables(&namespace).await.unwrap().into_iter().next() {
            found = Some((namespace, table));
            break;
        }
    }
    let (namespace, name) = found.expect("the catalog has no tables");
    let table = catalog.table(&namespace, &name).await.unwrap();
    assert_ne!(table.fields.len(), 0);
    let snapshot = table.current_snapshot.expect("the table has no snapshot");
    let manifests = table.manifests(snapshot).await.unwrap();
    assert_ne!(manifests.len(), 0);
    let files = table.snapshot_files(snapshot).await.unwrap();
    assert!(files.iter().any(|file| file.content == "data"));
    for file in &files {
        eprintln!(
            "{} {} {} {}",
            file.status, file.content, file.partition, file.record_count
        );
    }
}
