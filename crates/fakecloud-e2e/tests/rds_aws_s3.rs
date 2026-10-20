//! End-to-end test for the RDS PostgreSQL `aws_s3` extension.
//!
//! Drives the round trip: put a CSV in S3, create a Postgres DB
//! instance, `CREATE EXTENSION aws_s3 CASCADE`, import the CSV into a
//! table, then export a query back into S3 and read it from the bucket.

mod helpers;

use aws_sdk_s3::primitives::ByteStream;
use helpers::TestServer;

async fn connect_with_retry(
    host: &str,
    port: i32,
    user: &str,
    password: &str,
    dbname: &str,
) -> tokio_postgres::Client {
    let connection_string =
        format!("host={host} port={port} user={user} password={password} dbname={dbname}");
    let mut last_error = None;
    for _ in 0..30 {
        match tokio_postgres::connect(&connection_string, helpers::pg_tls()).await {
            Ok((client, connection)) => {
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                return client;
            }
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    panic!(
        "could not connect to postgres at {}:{}: {:?}",
        host, port, last_error
    );
}

#[tokio::test]
async fn aws_s3_extension_import_export_round_trip() {
    // Force the postgres image to be (re)built locally so the aws_s3
    // extension files baked into this commit are present. The published
    // `fakecloud-postgres:<major>-<release>` image only ships extensions
    // that existed at the corresponding release tag; this test runs on
    // pre-release commits that add aws_s3 before the next tag goes out.
    let server = TestServer::start_with_env(&[("FAKECLOUD_REBUILD_POSTGRES_IMAGE", "1")]).await;
    let s3 = server.s3_client().await;
    let rds = server.rds_client().await;

    // 1. Bucket + CSV input.
    s3.create_bucket()
        .bucket("aws-s3-ext")
        .send()
        .await
        .expect("create bucket");
    let csv = b"1,alice\n2,bob\n3,carol\n";
    s3.put_object()
        .bucket("aws-s3-ext")
        .key("input.csv")
        .body(ByteStream::from_static(csv))
        .send()
        .await
        .expect("put input.csv");

    // 2. Postgres instance — reuses the lazy fakecloud-postgres image
    //    that the aws_lambda e2e already exercises, so this run is fast
    //    when ordered after the lambda test on the same runner.
    rds.create_db_instance()
        .db_instance_identifier("aws-s3-ext-db")
        .allocated_storage(20)
        .db_instance_class("db.t3.micro")
        .engine("postgres")
        .engine_version("16.3")
        .master_username("admin")
        .master_user_password("secret123")
        .db_name("appdb")
        .send()
        .await
        .expect("create postgres instance");
    let instance = helpers::wait_for_db_available(&rds, "aws-s3-ext-db", 240).await;
    let endpoint = instance.endpoint().expect("endpoint");
    let host = endpoint.address().expect("address").to_string();
    let port = endpoint.port().expect("port");

    let client = connect_with_retry(&host, port, "admin", "secret123", "appdb").await;
    client
        .simple_query("CREATE EXTENSION IF NOT EXISTS aws_s3 CASCADE")
        .await
        .expect("load aws_s3 extension");
    client
        .simple_query("CREATE TABLE people (id int, name text)")
        .await
        .expect("create table");

    // 3. table_import_from_s3 (positional + composite overloads).
    let row = client
        .query_one(
            "SELECT rows_imported, bytes_processed FROM aws_s3.table_import_from_s3(\
                'people', '', 'format csv', 'aws-s3-ext', 'input.csv', 'us-east-1')",
            &[],
        )
        .await
        .expect("import csv");
    let rows_imported: i64 = row.get(0);
    let bytes_processed: i64 = row.get(1);
    assert_eq!(rows_imported, 3);
    assert_eq!(bytes_processed, csv.len() as i64);

    let count_row = client
        .query_one("SELECT count(*)::int FROM people", &[])
        .await
        .unwrap();
    let count: i32 = count_row.get(0);
    assert_eq!(count, 3);

    // 4. query_export_to_s3 — runs against the deterministic 3-row state
    //    before we exercise the composite-typed import overload (which
    //    appends a second copy of the CSV into the same table).
    let export_row = client
        .query_one(
            "SELECT rows_uploaded, files_uploaded FROM aws_s3.query_export_to_s3(\
                'SELECT id, name FROM people ORDER BY id LIMIT 2', \
                aws_commons.create_s3_uri('aws-s3-ext', 'export.csv', 'us-east-1'), \
                'format csv')",
            &[],
        )
        .await
        .expect("export query");
    let rows_uploaded: i64 = export_row.get(0);
    let files_uploaded: i64 = export_row.get(1);
    assert_eq!(rows_uploaded, 2);
    assert_eq!(files_uploaded, 1);

    // 5. Composite-typed import overload — into a fresh scratch table so
    //    the export above stays deterministic.
    client
        .simple_query("CREATE TABLE people_scratch (id int, name text)")
        .await
        .expect("create scratch table");
    let composite_row = client
        .query_one(
            "SELECT rows_imported FROM aws_s3.table_import_from_s3(\
                'people_scratch', '', 'format csv', \
                aws_commons.create_s3_uri('aws-s3-ext', 'input.csv', 'us-east-1'))",
            &[],
        )
        .await
        .expect("import via composite uri");
    let rows: i64 = composite_row.get(0);
    assert_eq!(rows, 3);

    let exported = s3
        .get_object()
        .bucket("aws-s3-ext")
        .key("export.csv")
        .send()
        .await
        .expect("get export.csv");
    let body = exported.body.collect().await.expect("collect").into_bytes();
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(
        body_str.contains("1,alice") && body_str.contains("2,bob"),
        "exported csv missing rows: {body_str}"
    );
    assert!(!body_str.contains("3,carol"));

    let scratch_count_row = client
        .query_one("SELECT count(*)::int FROM people_scratch", &[])
        .await
        .unwrap();
    let scratch_count: i32 = scratch_count_row.get(0);
    assert_eq!(scratch_count, 3);
}
