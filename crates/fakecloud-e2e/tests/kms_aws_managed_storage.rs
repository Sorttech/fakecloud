//! Storage encrypted without a caller-named key reports the account's
//! AWS-managed KMS key: `alias/aws/rds` for RDS, DocumentDB and Neptune,
//! `alias/aws/ebs` for EBS. The reported ARN is exactly the key
//! `DescribeKey alias/aws/<service>` resolves to, it follows the resource into
//! its snapshots, copies and restores, and CloudFormation reports the same key
//! as the API.

mod helpers;

use aws_sdk_ec2::types::{BlockDeviceMapping, EbsBlockDevice, Filter, InstanceType, VolumeType};
use helpers::TestServer;

/// The ARN of the key `alias` (`alias/aws/<service>`) targets, per KMS.
async fn managed_key_arn(kms: &aws_sdk_kms::Client, alias: &str) -> String {
    let meta = kms
        .describe_key()
        .key_id(alias)
        .send()
        .await
        .unwrap_or_else(|e| panic!("DescribeKey {alias}: {e:?}"))
        .key_metadata
        .expect("key metadata");
    assert_eq!(
        meta.key_manager().map(|m| m.as_str()),
        Some("AWS"),
        "{alias} is not AWS-managed"
    );
    meta.arn().expect("key arn").to_string()
}

#[tokio::test]
async fn rds_docdb_neptune_encrypted_storage_reports_the_aws_rds_key() {
    let server = TestServer::start().await;
    let cfg = server.aws_config().await;
    let kms = aws_sdk_kms::Client::new(&cfg);
    let rds = server.rds_client().await;

    // RDS cluster: StorageEncrypted without KmsKeyId.
    let cluster = rds
        .create_db_cluster()
        .db_cluster_identifier("kms-aurora")
        .engine("aurora-postgresql")
        .master_username("postgres")
        .master_user_password("Passw0rd!")
        .storage_encrypted(true)
        .send()
        .await
        .expect("create db cluster")
        .db_cluster
        .expect("cluster");
    let key = cluster.kms_key_id().expect("KmsKeyId").to_string();
    assert_eq!(key, managed_key_arn(&kms, "alias/aws/rds").await);

    let described = rds
        .describe_db_clusters()
        .db_cluster_identifier("kms-aurora")
        .send()
        .await
        .expect("describe db clusters");
    assert_eq!(described.db_clusters()[0].kms_key_id(), Some(key.as_str()));

    // Its snapshot and a restore of it carry the key.
    let snap = rds
        .create_db_cluster_snapshot()
        .db_cluster_snapshot_identifier("kms-aurora-snap")
        .db_cluster_identifier("kms-aurora")
        .send()
        .await
        .expect("create db cluster snapshot")
        .db_cluster_snapshot
        .expect("snapshot");
    assert_eq!(snap.kms_key_id(), Some(key.as_str()));
    assert_eq!(snap.storage_encrypted(), Some(true));
    let restored = rds
        .restore_db_cluster_from_snapshot()
        .db_cluster_identifier("kms-aurora-restored")
        .snapshot_identifier("kms-aurora-snap")
        .engine("aurora-postgresql")
        .send()
        .await
        .expect("restore db cluster")
        .db_cluster
        .expect("restored cluster");
    let restored = rds
        .describe_db_clusters()
        .db_cluster_identifier(restored.db_cluster_identifier().unwrap())
        .send()
        .await
        .expect("describe restored");
    assert_eq!(restored.db_clusters()[0].kms_key_id(), Some(key.as_str()));

    // RDS instance: StorageEncrypted without KmsKeyId (the record, not the
    // backing container, is what reports the key).
    let instance = rds
        .create_db_instance()
        .db_instance_identifier("kms-postgres")
        .db_instance_class("db.t3.micro")
        .engine("postgres")
        .master_username("admin")
        .master_user_password("secret123")
        .allocated_storage(20)
        .storage_encrypted(true)
        .send()
        .await
        .expect("create db instance")
        .db_instance
        .expect("instance");
    assert_eq!(instance.storage_encrypted(), Some(true));
    assert_eq!(instance.kms_key_id(), Some(key.as_str()));
    let described = rds
        .describe_db_instances()
        .db_instance_identifier("kms-postgres")
        .send()
        .await
        .expect("describe db instances");
    assert_eq!(described.db_instances()[0].kms_key_id(), Some(key.as_str()));
    rds.delete_db_instance()
        .db_instance_identifier("kms-postgres")
        .skip_final_snapshot(true)
        .send()
        .await
        .expect("delete db instance");

    // DocumentDB and Neptune encrypt with the same `aws/rds` key.
    let docdb = server.docdb_client().await;
    let doc = docdb
        .create_db_cluster()
        .db_cluster_identifier("kms-docdb")
        .engine("docdb")
        .master_username("docdbadmin")
        .master_user_password("Passw0rd!")
        .storage_encrypted(true)
        .send()
        .await
        .expect("create docdb cluster")
        .db_cluster
        .expect("docdb cluster");
    assert_eq!(doc.kms_key_id(), Some(key.as_str()));

    let neptune = server.neptune_client().await;
    let graph = neptune
        .create_db_cluster()
        .db_cluster_identifier("kms-neptune")
        .engine("neptune")
        .storage_encrypted(true)
        .send()
        .await
        .expect("create neptune cluster")
        .db_cluster
        .expect("neptune cluster");
    assert_eq!(graph.kms_key_id(), Some(key.as_str()));
    let member = neptune
        .create_db_instance()
        .db_instance_identifier("kms-neptune-1")
        .db_instance_class("db.r5.large")
        .engine("neptune")
        .db_cluster_identifier("kms-neptune")
        .send()
        .await
        .expect("create neptune instance")
        .db_instance
        .expect("neptune instance");
    assert_eq!(member.kms_key_id(), Some(key.as_str()));
}

#[tokio::test]
async fn ebs_encryption_reports_the_aws_ebs_key() {
    let server = TestServer::start().await;
    let cfg = server.aws_config().await;
    let kms = aws_sdk_kms::Client::new(&cfg);
    let ec2 = aws_sdk_ec2::Client::new(&cfg);

    // Encrypted without KmsKeyId.
    let vol = ec2
        .create_volume()
        .availability_zone("us-east-1a")
        .size(12)
        .volume_type(VolumeType::Gp3)
        .encrypted(true)
        .send()
        .await
        .expect("create volume");
    let key = vol.kms_key_id().expect("KmsKeyId").to_string();
    assert_eq!(key, managed_key_arn(&kms, "alias/aws/ebs").await);

    // The account's default key is that key ARN, not the alias.
    let default = ec2
        .get_ebs_default_kms_key_id()
        .send()
        .await
        .expect("get default key");
    assert_eq!(default.kms_key_id(), Some(key.as_str()));

    // A snapshot inherits the volume's key, and a volume restored from it
    // is encrypted with the same key.
    let snap = ec2
        .create_snapshot()
        .volume_id(vol.volume_id().unwrap())
        .send()
        .await
        .expect("create snapshot");
    assert_eq!(snap.encrypted(), Some(true));
    assert_eq!(snap.kms_key_id(), Some(key.as_str()));
    let restored = ec2
        .create_volume()
        .availability_zone("us-east-1a")
        .snapshot_id(snap.snapshot_id().unwrap())
        .send()
        .await
        .expect("create volume from snapshot");
    assert_eq!(restored.encrypted(), Some(true));
    assert_eq!(restored.size(), Some(12));
    assert_eq!(restored.kms_key_id(), Some(key.as_str()));

    // Encrypting a copy of an unencrypted snapshot uses the default key.
    let plain = ec2
        .create_volume()
        .availability_zone("us-east-1a")
        .size(4)
        .send()
        .await
        .expect("create plain volume");
    assert_eq!(plain.kms_key_id(), None);
    let plain_snap = ec2
        .create_snapshot()
        .volume_id(plain.volume_id().unwrap())
        .send()
        .await
        .expect("snapshot plain volume");
    let copy = ec2
        .copy_snapshot()
        .source_region("us-east-1")
        .source_snapshot_id(plain_snap.snapshot_id().unwrap())
        .encrypted(true)
        .send()
        .await
        .expect("copy snapshot");
    let copies = ec2
        .describe_snapshots()
        .snapshot_ids(copy.snapshot_id().unwrap())
        .send()
        .await
        .expect("describe copy");
    assert_eq!(copies.snapshots()[0].encrypted(), Some(true));
    assert_eq!(copies.snapshots()[0].kms_key_id(), Some(key.as_str()));

    // Encryption by default encrypts a volume nobody asked to encrypt.
    ec2.enable_ebs_encryption_by_default()
        .send()
        .await
        .expect("enable encryption by default");
    let by_default = ec2
        .create_volume()
        .availability_zone("us-east-1a")
        .size(4)
        .send()
        .await
        .expect("create default-encrypted volume");
    assert_eq!(by_default.encrypted(), Some(true));
    assert_eq!(by_default.kms_key_id(), Some(key.as_str()));
    ec2.disable_ebs_encryption_by_default()
        .send()
        .await
        .expect("disable encryption by default");

    // RunInstances block-device mappings create encrypted, attached volumes.
    let run = ec2
        .run_instances()
        .image_id("ami-12345678")
        .instance_type(InstanceType::T3Micro)
        .min_count(1)
        .max_count(1)
        .block_device_mappings(
            BlockDeviceMapping::builder()
                .device_name("/dev/xvda")
                .ebs(
                    EbsBlockDevice::builder()
                        .volume_size(16)
                        .encrypted(true)
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .expect("run instances");
    let instance_id = run.instances()[0].instance_id().unwrap().to_string();
    let launched = ec2
        .describe_volumes()
        .filters(
            Filter::builder()
                .name("attachment.instance-id")
                .values(&instance_id)
                .build(),
        )
        .send()
        .await
        .expect("describe launch volumes");
    let root = &launched.volumes()[0];
    assert_eq!(launched.volumes().len(), 1);
    assert_eq!(root.size(), Some(16));
    assert_eq!(root.encrypted(), Some(true));
    assert_eq!(root.kms_key_id(), Some(key.as_str()));
    let described = ec2
        .describe_instances()
        .instance_ids(&instance_id)
        .send()
        .await
        .expect("describe instances");
    let mappings = described.reservations()[0].instances()[0].block_device_mappings();
    assert_eq!(mappings.len(), 1);
    assert_eq!(
        mappings[0].ebs().and_then(|e| e.volume_id()),
        root.volume_id()
    );
}

const STACK: &str = r#"{
  "Resources": {
    "Aurora": {
      "Type": "AWS::RDS::DBCluster",
      "Properties": {
        "DBClusterIdentifier": "cfn-kms-aurora",
        "Engine": "aurora-postgresql",
        "MasterUsername": "postgres",
        "MasterUserPassword": "Passw0rd!",
        "StorageEncrypted": true
      }
    },
    "Doc": {
      "Type": "AWS::DocDB::DBCluster",
      "Properties": {
        "DBClusterIdentifier": "cfn-kms-docdb",
        "MasterUsername": "docdbadmin",
        "MasterUserPassword": "Passw0rd!",
        "StorageEncrypted": true
      }
    },
    "Graph": {
      "Type": "AWS::Neptune::DBCluster",
      "Properties": {
        "DBClusterIdentifier": "cfn-kms-neptune",
        "StorageEncrypted": true
      }
    },
    "Vol": {
      "Type": "AWS::EC2::Volume",
      "Properties": {
        "AvailabilityZone": "us-east-1a",
        "Size": 8,
        "Encrypted": true
      }
    }
  },
  "Outputs": {
    "VolumeId": { "Value": { "Ref": "Vol" } }
  }
}"#;

#[tokio::test]
async fn stack_storage_reports_the_same_aws_managed_keys_as_the_api() {
    let server = TestServer::start().await;
    let cfg = server.aws_config().await;
    let kms = aws_sdk_kms::Client::new(&cfg);
    let cfn = server.cloudformation_client().await;
    cfn.create_stack()
        .stack_name("kms-storage")
        .template_body(STACK)
        .send()
        .await
        .expect("create stack");
    let stacks = cfn
        .describe_stacks()
        .stack_name("kms-storage")
        .send()
        .await
        .expect("describe stacks");
    let stack = &stacks.stacks()[0];
    assert_eq!(
        stack.stack_status().map(|s| s.as_str()),
        Some("CREATE_COMPLETE"),
        "{:?}",
        stack.stack_status_reason()
    );
    let rds_key = managed_key_arn(&kms, "alias/aws/rds").await;

    let rds = server.rds_client().await;
    let aurora = rds
        .describe_db_clusters()
        .db_cluster_identifier("cfn-kms-aurora")
        .send()
        .await
        .expect("describe aurora");
    assert_eq!(aurora.db_clusters()[0].kms_key_id(), Some(rds_key.as_str()));

    let docdb = server.docdb_client().await;
    let doc = docdb
        .describe_db_clusters()
        .db_cluster_identifier("cfn-kms-docdb")
        .send()
        .await
        .expect("describe docdb");
    assert_eq!(doc.db_clusters()[0].kms_key_id(), Some(rds_key.as_str()));

    let neptune = server.neptune_client().await;
    let graph = neptune
        .describe_db_clusters()
        .db_cluster_identifier("cfn-kms-neptune")
        .send()
        .await
        .expect("describe neptune");
    assert_eq!(graph.db_clusters()[0].kms_key_id(), Some(rds_key.as_str()));

    let volume_id = stack
        .outputs()
        .iter()
        .find(|o| o.output_key() == Some("VolumeId"))
        .and_then(|o| o.output_value())
        .expect("VolumeId output")
        .to_string();
    let ec2 = aws_sdk_ec2::Client::new(&cfg);
    let vols = ec2
        .describe_volumes()
        .volume_ids(&volume_id)
        .send()
        .await
        .expect("describe stack volume");
    assert_eq!(vols.volumes()[0].encrypted(), Some(true));
    assert_eq!(
        vols.volumes()[0].kms_key_id(),
        Some(managed_key_arn(&kms, "alias/aws/ebs").await.as_str())
    );
}
