mod helpers;

use helpers::TestServer;

async fn connect_postgres_with_retry(
    host: &str,
    port: i32,
    user: &str,
    password: &str,
    dbname: &str,
) -> Result<tokio_postgres::Client, tokio_postgres::Error> {
    let connection_string =
        format!("host={host} port={port} user={user} password={password} dbname={dbname}");
    let mut last_error = None;
    for _ in 0..40 {
        match tokio_postgres::connect(&connection_string, helpers::pg_tls()).await {
            Ok((client, conn)) => {
                tokio::spawn(conn);
                return Ok(client);
            }
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    Err(last_error.expect("postgres connection error"))
}

/// DB instances survive a restart. Reproduces issue #914: a DB instance
/// created with `aws rds create-db-instance` disappeared after restart
/// because the background container-start task flipped status to
/// `available` without persisting, and the load path then dropped the
/// row as a "stuck creating" placeholder.
///
/// Uses `start_full` rather than `start_persistent` because the latter
/// disables the container CLI (most persistence tests cover metadata-
/// only ops); CreateDBInstance needs real Docker.
#[tokio::test]
async fn persistence_round_trip_db_instance() {
    let tmp = tempfile::tempdir().unwrap();
    let data_path = tmp.path().display().to_string();
    let extra_args = ["--storage-mode", "persistent", "--data-path", &data_path];
    // Drop the instance's durable data volume even if the test fails.
    let _volumes = helpers::DataVolumeGuard::new(tmp.path());
    let mut server = TestServer::start_full(&[], &extra_args).await;
    let client = server.rds_client().await;

    client
        .create_db_instance()
        .db_instance_identifier("persist-db")
        .allocated_storage(20)
        .db_instance_class("db.t3.micro")
        .engine("postgres")
        .engine_version("16.3")
        .master_username("admin")
        .master_user_password("secret123")
        .db_name("appdb")
        .send()
        .await
        .unwrap();

    let _ = helpers::wait_for_db_available(&client, "persist-db", 180).await;

    drop(client);
    server.restart().await;
    let client = server.rds_client().await;

    // Container recovery is async (#1338): the row reloads as
    // `starting`, then flips back to `available` once the container is
    // healthy. Wait rather than asserting an instantaneous status.
    let inst = helpers::wait_for_db_available(&client, "persist-db", 240).await;
    assert_eq!(inst.db_instance_identifier(), Some("persist-db"));
    assert_eq!(inst.engine(), Some("postgres"));
    assert_eq!(inst.master_username(), Some("admin"));
    assert_eq!(inst.db_instance_status(), Some("available"));
}

/// Backing container is recreated on restart so the persisted DB
/// endpoint is actually usable. Reproduces issue #1338: pre-fix,
/// `DescribeDBInstances` returned `available` but `tokio_postgres::connect`
/// failed because the container was gone.
#[tokio::test]
async fn persistence_db_endpoint_works_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let data_path = tmp.path().display().to_string();
    let extra_args = ["--storage-mode", "persistent", "--data-path", &data_path];
    // Drop the instance's durable data volume even if the test fails.
    let _volumes = helpers::DataVolumeGuard::new(tmp.path());
    let mut server = TestServer::start_full(&[], &extra_args).await;
    let client = server.rds_client().await;

    client
        .create_db_instance()
        .db_instance_identifier("restart-db")
        .allocated_storage(20)
        .db_instance_class("db.t3.micro")
        .engine("postgres")
        .engine_version("16.3")
        .master_username("admin")
        .master_user_password("secret123")
        .db_name("appdb")
        .send()
        .await
        .unwrap();
    let _ = helpers::wait_for_db_available(&client, "restart-db", 240).await;

    drop(client);
    server.restart().await;
    let client = server.rds_client().await;

    let inst = helpers::wait_for_db_available(&client, "restart-db", 240).await;
    let endpoint = inst.endpoint().expect("endpoint");
    let host = endpoint.address().expect("address");
    let port = endpoint.port().expect("port");

    let db = connect_postgres_with_retry(host, port, "admin", "secret123", "appdb")
        .await
        .expect("connect to recovered postgres container");
    let row = db.query_one("SELECT 1", &[]).await.expect("select 1");
    let value: i32 = row.get(0);
    assert_eq!(value, 1);
}

/// StopDBInstance and StartDBInstance actually toggle the backing
/// container, not just emit events. Pre-fix the ops were XML stubs and
/// the container kept running regardless of the reported status.
#[tokio::test]
async fn start_stop_db_instance_toggles_backing_container() {
    let tmp = tempfile::tempdir().unwrap();
    let data_path = tmp.path().display().to_string();
    let extra_args = ["--storage-mode", "persistent", "--data-path", &data_path];
    // Drop the instance's durable data volume even if the test fails.
    let _volumes = helpers::DataVolumeGuard::new(tmp.path());
    let server = TestServer::start_full(&[], &extra_args).await;
    let client = server.rds_client().await;

    client
        .create_db_instance()
        .db_instance_identifier("toggle-db")
        .allocated_storage(20)
        .db_instance_class("db.t3.micro")
        .engine("postgres")
        .engine_version("16.3")
        .master_username("admin")
        .master_user_password("secret123")
        .db_name("appdb")
        .send()
        .await
        .unwrap();
    let inst = helpers::wait_for_db_available(&client, "toggle-db", 240).await;
    let endpoint = inst.endpoint().expect("endpoint");
    let host = endpoint.address().expect("address").to_string();
    let port_before = endpoint.port().expect("port");

    // Sanity: endpoint works before stopping.
    let _ = connect_postgres_with_retry(&host, port_before, "admin", "secret123", "appdb")
        .await
        .expect("connect before stop");

    client
        .stop_db_instance()
        .db_instance_identifier("toggle-db")
        .send()
        .await
        .unwrap();

    let stopped = client
        .describe_db_instances()
        .db_instance_identifier("toggle-db")
        .send()
        .await
        .unwrap();
    assert_eq!(
        stopped.db_instances()[0].db_instance_status(),
        Some("stopped"),
    );
    // Endpoint must not be reachable now.
    let connect = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio_postgres::connect(
            &format!("host={host} port={port_before} user=admin password=secret123 dbname=appdb"),
            helpers::pg_tls(),
        ),
    )
    .await;
    assert!(
        matches!(connect, Err(_) | Ok(Err(_))),
        "stopped DB endpoint should not accept connections",
    );

    client
        .start_db_instance()
        .db_instance_identifier("toggle-db")
        .send()
        .await
        .unwrap();
    let restarted = helpers::wait_for_db_available(&client, "toggle-db", 240).await;
    let endpoint = restarted.endpoint().expect("endpoint");
    let host_after = endpoint.address().expect("address");
    let port_after = endpoint.port().expect("port");

    let db = connect_postgres_with_retry(host_after, port_after, "admin", "secret123", "appdb")
        .await
        .expect("connect after start");
    let row = db.query_one("SELECT 1", &[]).await.expect("select 1");
    let value: i32 = row.get(0);
    assert_eq!(value, 1);
}

/// Data WRITTEN to a DB survives a restart, not just the endpoint. With
/// persistent mode now defaulting DB data volumes on, a row inserted before
/// restart is still readable after the backing container is recreated. Without
/// the durable volume the recovered postgres container would come back empty
/// and the row would be gone -- this is the data-plane half of persistence.
#[tokio::test]
async fn persistence_db_row_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let data_path = tmp.path().display().to_string();
    let extra_args = ["--storage-mode", "persistent", "--data-path", &data_path];
    // Drop the instance's durable data volume even if the test fails.
    let volume_guard = helpers::DataVolumeGuard::new(tmp.path());
    let mut server = TestServer::start_full(&[], &extra_args).await;
    let client = server.rds_client().await;

    client
        .create_db_instance()
        .db_instance_identifier("rowsurvive-db")
        .allocated_storage(20)
        .db_instance_class("db.t3.micro")
        .engine("postgres")
        .engine_version("16.3")
        .master_username("admin")
        .master_user_password("secret123")
        .db_name("appdb")
        .send()
        .await
        .unwrap();
    let inst = helpers::wait_for_db_available(&client, "rowsurvive-db", 240).await;
    let endpoint = inst.endpoint().expect("endpoint");
    let host = endpoint.address().expect("address").to_string();
    let port = endpoint.port().expect("port");

    // Seed a row before the restart.
    {
        let db = connect_postgres_with_retry(&host, port, "admin", "secret123", "appdb")
            .await
            .expect("connect to seed");
        db.batch_execute(
            "CREATE TABLE durable (id int primary key, note text); \
             INSERT INTO durable (id, note) VALUES (1, 'survives-restart');",
        )
        .await
        .expect("seed row");
    }

    drop(client);
    server.restart().await;
    let client = server.rds_client().await;

    let inst = helpers::wait_for_db_available(&client, "rowsurvive-db", 240).await;
    let endpoint = inst.endpoint().expect("endpoint");
    let host = endpoint.address().expect("address").to_string();
    let port = endpoint.port().expect("port");

    let db = connect_postgres_with_retry(&host, port, "admin", "secret123", "appdb")
        .await
        .expect("connect to recovered container");
    let row = db
        .query_one("SELECT note FROM durable WHERE id = 1", &[])
        .await
        .expect("row must survive the restart via the durable data volume");
    let note: String = row.get(0);
    assert_eq!(note, "survives-restart");

    // The volume is scoped to this data dir and DeleteDBInstance drops it, so
    // the test leaves nothing behind for a later run to inherit (#2630).
    let volumes = volume_guard.volumes(&["fakecloud-rds=rowsurvive-db"]);
    assert_eq!(volumes.len(), 1, "one scoped volume: {volumes:?}");
    drop(db);
    delete_db_and_wait_volume_gone(&client, "rowsurvive-db", &volumes[0]).await;
}

/// DeleteDBInstance (skipping the final snapshot), then wait for its data
/// volume to be removed by the backgrounded teardown.
async fn delete_db_and_wait_volume_gone(
    client: &aws_sdk_rds::Client,
    identifier: &str,
    volume: &str,
) {
    client
        .delete_db_instance()
        .db_instance_identifier(identifier)
        .skip_final_snapshot(true)
        .send()
        .await
        .unwrap();
    for _ in 0..120 {
        if !helpers::volume_exists(volume) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("DeleteDBInstance left data volume {volume} behind");
}

async fn create_postgres(client: &aws_sdk_rds::Client, identifier: &str) -> (String, i32) {
    client
        .create_db_instance()
        .db_instance_identifier(identifier)
        .allocated_storage(20)
        .db_instance_class("db.t3.micro")
        .engine("postgres")
        .engine_version("16.3")
        .master_username("admin")
        .master_user_password("secret123")
        .db_name("appdb")
        .send()
        .await
        .unwrap();
    endpoint_of(client, identifier).await
}

async fn endpoint_of(client: &aws_sdk_rds::Client, identifier: &str) -> (String, i32) {
    let inst = helpers::wait_for_db_available(client, identifier, 240).await;
    let endpoint = inst.endpoint().expect("endpoint");
    (
        endpoint.address().expect("address").to_string(),
        endpoint.port().expect("port"),
    )
}

/// Issue #2630: the durable volume used to be named after the account and
/// identifier only, so a second fakecloud on a fresh `--data-path` creating a
/// DB with the same identifier attached the first one's database. Volumes are
/// now scoped to the data dir, so the second DB starts empty.
#[tokio::test]
async fn fresh_data_dir_does_not_inherit_another_dirs_database() {
    let id = "isolated-db";

    let tmp_a = tempfile::tempdir().unwrap();
    let path_a = tmp_a.path().display().to_string();
    let volumes_a = helpers::DataVolumeGuard::new(tmp_a.path());
    {
        let args = ["--storage-mode", "persistent", "--data-path", &path_a];
        let server = TestServer::start_full(&[], &args).await;
        let client = server.rds_client().await;
        let (host, port) = create_postgres(&client, id).await;
        let db = connect_postgres_with_retry(&host, port, "admin", "secret123", "appdb")
            .await
            .expect("connect to first data dir's db");
        db.batch_execute("CREATE TABLE only_in_a (id int primary key);")
            .await
            .expect("seed first data dir");
        // Stop the first server; its data volume stays (persistent mode).
    }
    assert_eq!(
        volumes_a.volumes(&[&format!("fakecloud-rds={id}")]).len(),
        1
    );

    let tmp_b = tempfile::tempdir().unwrap();
    let path_b = tmp_b.path().display().to_string();
    let volumes_b = helpers::DataVolumeGuard::new(tmp_b.path());
    let args = ["--storage-mode", "persistent", "--data-path", &path_b];
    let server = TestServer::start_full(&[], &args).await;
    let client = server.rds_client().await;
    let (host, port) = create_postgres(&client, id).await;
    let db = connect_postgres_with_retry(&host, port, "admin", "secret123", "appdb")
        .await
        .expect("connect to second data dir's db");
    let exists: bool = db
        .query_one("SELECT to_regclass('public.only_in_a') IS NOT NULL", &[])
        .await
        .expect("query catalog")
        .get(0);
    assert!(
        !exists,
        "a fresh data dir must not inherit another data dir's database"
    );
    // Each data dir has its own volume for the identifier.
    let names_b = volumes_b.volumes(&[&format!("fakecloud-rds={id}")]);
    assert_eq!(names_b.len(), 1);
    assert!(!volumes_a
        .volumes(&[&format!("fakecloud-rds={id}")])
        .contains(&names_b[0]));
}

/// Remove every key named in `keys`, at any depth, so a snapshot reads as one
/// written before those fields existed.
fn strip_keys(value: &mut serde_json::Value, keys: &[&str]) {
    match value {
        serde_json::Value::Object(map) => {
            for key in keys {
                map.remove(*key);
            }
            for v in map.values_mut() {
                strip_keys(v, keys);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                strip_keys(v, keys);
            }
        }
        _ => {}
    }
}

/// An instance persisted by a build from before data-dir scoping keeps the
/// unscoped (legacy) volume it was created with: after the upgrade its rows
/// are still there, and DeleteDBInstance removes that legacy volume.
#[tokio::test]
async fn pre_scoping_instance_keeps_its_legacy_volume() {
    // Unique per run: the legacy name is global to the daemon, so a fixed one
    // could collide with (and this test remove) a real volume.
    let id = format!("legacy-adopt-{}", uuid::Uuid::new_v4().simple());
    let id = id.as_str();
    let legacy = format!("fakecloud-rds-data-123456789012-{id}");
    let cli = helpers::container_cli();
    let tmp = tempfile::tempdir().unwrap();
    let data_path = tmp.path().display().to_string();
    let args = ["--storage-mode", "persistent", "--data-path", &data_path];
    let mut volumes = helpers::DataVolumeGuard::new(tmp.path());
    volumes.also_remove(&legacy);
    // A crashed earlier run may have left it; start from a known state.
    let _ = std::process::Command::new(&cli)
        .args(["volume", "rm", "-f", &legacy])
        .output();

    {
        let server = TestServer::start_full(&[], &args).await;
        let client = server.rds_client().await;
        let (host, port) = create_postgres(&client, id).await;
        let db = connect_postgres_with_retry(&host, port, "admin", "secret123", "appdb")
            .await
            .expect("connect to seed");
        db.batch_execute(
            "CREATE TABLE durable (id int primary key, note text); \
             INSERT INTO durable VALUES (1, 'from-before-the-upgrade');",
        )
        .await
        .expect("seed row");
    }

    // Turn the data dir into what a pre-scoping build left: the database in
    // the unscoped volume, and state without the scoping fields.
    let scoped = volumes.volumes(&[&format!("fakecloud-rds={id}")]);
    assert_eq!(scoped.len(), 1, "scoped volume: {scoped:?}");
    let copy = std::process::Command::new(&cli)
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{}:/from:ro", scoped[0]),
            "-v",
            &format!("{legacy}:/to"),
            "alpine:3",
            "sh",
            "-c",
            "cp -a /from/. /to/",
        ])
        .output()
        .expect("spawn copy container");
    assert!(
        copy.status.success(),
        "copy into legacy volume: {}",
        String::from_utf8_lossy(&copy.stderr)
    );
    let removed = std::process::Command::new(&cli)
        .args(["volume", "rm", &scoped[0]])
        .output()
        .unwrap();
    assert!(removed.status.success(), "remove scoped volume");
    let snapshot = tmp.path().join("rds").join("snapshot.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&snapshot).expect("read rds snapshot")).unwrap();
    strip_keys(&mut json, &["data_volume"]);
    std::fs::write(&snapshot, serde_json::to_vec(&json).unwrap()).unwrap();

    // The upgraded server adopts the legacy volume for the persisted instance.
    let server = TestServer::start_full(&[], &args).await;
    let client = server.rds_client().await;
    let (host, port) = endpoint_of(&client, id).await;
    let db = connect_postgres_with_retry(&host, port, "admin", "secret123", "appdb")
        .await
        .expect("connect to recovered container");
    let note: String = db
        .query_one("SELECT note FROM durable WHERE id = 1", &[])
        .await
        .expect("row must survive the upgrade via the legacy volume")
        .get(0);
    assert_eq!(note, "from-before-the-upgrade");
    assert!(
        volumes
            .volumes(&[&format!("fakecloud-rds={id}")])
            .is_empty(),
        "no scoped volume is created while the legacy one is in use"
    );

    drop(db);
    delete_db_and_wait_volume_gone(&client, id, &legacy).await;
}

/// Parameter groups survive a restart.
#[tokio::test]
async fn persistence_round_trip_parameter_group() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let client = server.rds_client().await;

    client
        .create_db_parameter_group()
        .db_parameter_group_name("persist-pg")
        .db_parameter_group_family("postgres16")
        .description("Persistence test parameter group")
        .send()
        .await
        .unwrap();

    drop(client);
    server.restart().await;
    let client = server.rds_client().await;

    let groups = client
        .describe_db_parameter_groups()
        .db_parameter_group_name("persist-pg")
        .send()
        .await
        .unwrap();
    let pgs = groups.db_parameter_groups();
    assert!(
        pgs.iter()
            .any(|g| g.db_parameter_group_name() == Some("persist-pg")),
        "parameter group should survive restart"
    );
    let pg = pgs
        .iter()
        .find(|g| g.db_parameter_group_name() == Some("persist-pg"))
        .unwrap();
    assert_eq!(pg.db_parameter_group_family(), Some("postgres16"));
    assert_eq!(pg.description(), Some("Persistence test parameter group"));
}

/// Subnet groups survive a restart.
#[tokio::test]
async fn persistence_round_trip_subnet_group() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let vpc_subnets = server.default_subnet_ids().await;
    let client = server.rds_client().await;

    client
        .create_db_subnet_group()
        .db_subnet_group_name("persist-subnet-grp")
        .db_subnet_group_description("Persistence test subnet group")
        .subnet_ids(&vpc_subnets[0])
        .subnet_ids(&vpc_subnets[1])
        .send()
        .await
        .unwrap();

    drop(client);
    server.restart().await;
    let client = server.rds_client().await;

    let groups = client
        .describe_db_subnet_groups()
        .db_subnet_group_name("persist-subnet-grp")
        .send()
        .await
        .unwrap();
    let sgs = groups.db_subnet_groups();
    assert_eq!(sgs.len(), 1);
    let sg = &sgs[0];
    assert_eq!(sg.db_subnet_group_name(), Some("persist-subnet-grp"));
    assert_eq!(
        sg.db_subnet_group_description(),
        Some("Persistence test subnet group")
    );
    let subnet_ids: Vec<&str> = sg
        .subnets()
        .iter()
        .filter_map(|s| s.subnet_identifier())
        .collect();
    assert!(subnet_ids.contains(&vpc_subnets[0].as_str()));
    assert!(subnet_ids.contains(&vpc_subnets[1].as_str()));
}

/// Deletion survives a restart: a deleted parameter group does not reappear.
#[tokio::test]
async fn persistence_deletion_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let client = server.rds_client().await;

    client
        .create_db_parameter_group()
        .db_parameter_group_name("doomed-pg")
        .db_parameter_group_family("postgres16")
        .description("Will be deleted")
        .send()
        .await
        .unwrap();

    client
        .delete_db_parameter_group()
        .db_parameter_group_name("doomed-pg")
        .send()
        .await
        .unwrap();

    drop(client);
    server.restart().await;
    let client = server.rds_client().await;

    let groups = client.describe_db_parameter_groups().send().await.unwrap();
    assert!(
        !groups
            .db_parameter_groups()
            .iter()
            .any(|g| g.db_parameter_group_name() == Some("doomed-pg")),
        "deleted parameter group should not reappear"
    );
}

/// The data volume is keyed by the instance's `DbiResourceId`, not its
/// identifier: after a `NewDBInstanceIdentifier` rename and a restart the
/// renamed instance still has its rows, and a new instance created under the
/// old identifier starts empty instead of inheriting them.
#[tokio::test]
async fn renamed_instance_keeps_its_data_and_old_identifier_starts_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let data_path = tmp.path().display().to_string();
    let args = ["--storage-mode", "persistent", "--data-path", &data_path];
    let _volumes = helpers::DataVolumeGuard::new(tmp.path());
    let mut server = TestServer::start_full(&[], &args).await;
    let client = server.rds_client().await;

    let (host, port) = create_postgres(&client, "rename-src").await;
    {
        let db = connect_postgres_with_retry(&host, port, "admin", "secret123", "appdb")
            .await
            .expect("connect to seed");
        db.batch_execute(
            "CREATE TABLE durable (id int primary key, note text); \
             INSERT INTO durable VALUES (1, 'follows-the-rename');",
        )
        .await
        .expect("seed row");
    }
    client
        .modify_db_instance()
        .db_instance_identifier("rename-src")
        .new_db_instance_identifier("rename-dst")
        .apply_immediately(true)
        .send()
        .await
        .unwrap();

    drop(client);
    server.restart().await;
    let client = server.rds_client().await;

    let (host, port) = endpoint_of(&client, "rename-dst").await;
    let db = connect_postgres_with_retry(&host, port, "admin", "secret123", "appdb")
        .await
        .expect("connect to renamed instance");
    let note: String = db
        .query_one("SELECT note FROM durable WHERE id = 1", &[])
        .await
        .expect("renamed instance keeps its rows across a restart")
        .get(0);
    assert_eq!(note, "follows-the-rename");

    let (host, port) = create_postgres(&client, "rename-src").await;
    let fresh = connect_postgres_with_retry(&host, port, "admin", "secret123", "appdb")
        .await
        .expect("connect to new instance under the old identifier");
    let exists: bool = fresh
        .query_one("SELECT to_regclass('public.durable') IS NOT NULL", &[])
        .await
        .expect("query catalog")
        .get(0);
    assert!(
        !exists,
        "a new instance under the old identifier must not inherit the renamed one's data"
    );

    // The renamed instance kept its running container: stopping it through the
    // new identifier takes its endpoint down, while the new instance under the
    // old identifier keeps serving.
    client
        .stop_db_instance()
        .db_instance_identifier("rename-dst")
        .send()
        .await
        .unwrap();
    let stopped = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio_postgres::connect(
            &format!("host={host} port={port} user=admin password=secret123 dbname=appdb"),
            helpers::pg_tls(),
        ),
    )
    .await;
    assert!(
        matches!(stopped, Ok(Ok(_))),
        "stopping the renamed instance must not stop the new one under the old identifier"
    );
}

/// DeleteDBInstance with a final snapshot defers the source container's
/// teardown until the dump finishes. A new instance created under the same
/// identifier meanwhile must keep its own container and volume: the deferred
/// teardown targets the deleted instance only.
#[tokio::test]
async fn final_snapshot_teardown_spares_a_new_instance_with_the_same_identifier() {
    let tmp = tempfile::tempdir().unwrap();
    let data_path = tmp.path().display().to_string();
    let args = ["--storage-mode", "persistent", "--data-path", &data_path];
    let volumes = helpers::DataVolumeGuard::new(tmp.path());
    let server = TestServer::start_full(&[], &args).await;
    let client = server.rds_client().await;
    let id = "final-snap-db";

    let (host, port) = create_postgres(&client, id).await;
    {
        let db = connect_postgres_with_retry(&host, port, "admin", "secret123", "appdb")
            .await
            .expect("connect to seed");
        db.batch_execute("CREATE TABLE old_rows (id int primary key);")
            .await
            .expect("seed");
    }
    client
        .delete_db_instance()
        .db_instance_identifier(id)
        .final_db_snapshot_identifier("final-snap")
        .send()
        .await
        .unwrap();
    // Recreate right away, racing the deferred dump + teardown.
    let (host, port) = create_postgres(&client, id).await;

    // Wait for the final snapshot (and so the deferred teardown) to finish.
    let mut done = false;
    for _ in 0..240 {
        let snaps = client
            .describe_db_snapshots()
            .db_snapshot_identifier("final-snap")
            .send()
            .await
            .unwrap();
        if snaps.db_snapshots().first().and_then(|s| s.status()) == Some("available") {
            done = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(done, "final snapshot never became available");
    // The deferred teardown runs after the snapshot turns `available`: wait
    // for it to have removed the deleted instance's volume, leaving exactly
    // the new instance's.
    let mut remaining = Vec::new();
    for _ in 0..120 {
        remaining = volumes.volumes(&[&format!("fakecloud-rds={id}")]);
        if remaining.len() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert_eq!(remaining.len(), 1, "only the new instance's volume remains");

    let db = connect_postgres_with_retry(&host, port, "admin", "secret123", "appdb")
        .await
        .expect("the new instance must still be running after the deferred teardown");
    let exists: bool = db
        .query_one("SELECT to_regclass('public.old_rows') IS NOT NULL", &[])
        .await
        .expect("query catalog")
        .get(0);
    assert!(
        !exists,
        "the new instance starts with its own empty database"
    );
}
