# Iceberg catalogs

The **Iceberg** view in the sidebar shows the tables of an Iceberg REST catalog, with each table's snapshots, manifests and data files. It reads only and never changes the catalog. Add a catalog with **Add** at the top of the view, or "Add Iceberg catalog" in the command palette (`ctrl-shift-p`).

```text
lake                                   http://localhost:8181
└── demo
    └── services                       v2
        ├── location                   /data/lake/demo/services
        ├── Schema (4 columns)
        ├── Partitioned by identity(service_date)
        ├── Snapshots (3)
        │   └── 2026-11-24 10:40:00 UTC    delete · 1000 position deletes · current
        │       ├── id, parent, manifest list
        │       └── 0d279c25-…-m0.avro     1 delete file
        │           └── cdb91777-…-deletes.parquet   position deletes · service_date=2026-10-02 · 1000 rows · 1.6 KB
        ├── Properties (3)
        └── Metadata files (4)
```

| Field | What to enter |
| --- | --- |
| Name | how the catalog is shown, such as `lake` |
| REST catalog URI | such as `http://localhost:8181`; `https://` works too |
| Warehouse | only when the catalog serves several warehouses |
| Credential | a bearer token, or `client_id:client_secret` for the catalog's OAuth endpoint; kept in the system keychain. When editing, leave it empty to keep the stored one |

| To | Do |
| --- | --- |
| Follow a table's metadata | expand it, then Snapshots, a snapshot, a manifest |
| See every snapshot with its summary counts | right-click the table > Open Snapshots in Grid |
| List the files a snapshot reads | right-click the snapshot > Open Files in Grid |
| See the schema as a table | right-click the table > Open Schema in Grid |
| Copy a value, path or snapshot ID | right-click it |
| Read the catalog again | right-click the catalog > Reload; the table > Reload reads one table |

"Open in Grid" opens a new tab with the result, which you can filter, copy and export like a query result. The tab has no connection; pick one to run SQL in it.

A file's status is the manifest's: `added` in this snapshot, `existing` from an earlier one, or `removed`. A manifest marked "from an earlier snapshot" was carried over unchanged.

Only tables whose files are on local disk can be followed below the snapshot list: the catalog's own data (namespaces, tables, schema, snapshots) comes over HTTP, but manifests and data files are read from the paths the catalog gives. Tables on S3, ADLS or GCS show an error when a snapshot is expanded.

To try it, run the REST fixture with the warehouse at the same path inside and outside the container:

```sh
docker run -p 8181:8181 \
    -v /data/lake:/data/lake \
    -e CATALOG_WAREHOUSE=/data/lake \
    apache/iceberg-rest-fixture
```

and create a table from DuckDB, Spark or pyiceberg.
