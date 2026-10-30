//! CloudFormation provisioner for AWS::RDS::DBInstance and AWS::RDS::DBCluster
//! (BB16). Provisions both and asserts readback via the runtime RDS SDK.
//! Skips actual Docker container spawn — the CFN provisioner stages the
//! state record so DescribeDBInstances/DescribeDBClusters returns it; the
//! Engine/PG init that requires Docker is exercised by the dedicated RDS
//! e2e tests.

mod helpers;

use aws_sdk_cloudformation::types::{Capability, OnFailure};
use helpers::TestServer;

const TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "DbInstance": {
      "Type": "AWS::RDS::DBInstance",
      "Properties": {
        "DBInstanceIdentifier": "cfn-instance",
        "DBInstanceClass": "db.t4g.micro",
        "Engine": "postgres",
        "EngineVersion": "16.0",
        "MasterUsername": "admin",
        "MasterUserPassword": "hunter2-secret",
        "AllocatedStorage": "20",
        "Port": 5432,
        "PubliclyAccessible": false,
        "BackupRetentionPeriod": 7,
        "MultiAZ": true,
        "StorageEncrypted": true,
        "EnableIAMDatabaseAuthentication": true
      }
    },
    "DbCluster": {
      "Type": "AWS::RDS::DBCluster",
      "Properties": {
        "DBClusterIdentifier": "cfn-cluster",
        "Engine": "aurora-postgresql",
        "EngineVersion": "16.1",
        "MasterUsername": "admin",
        "MasterUserPassword": "cluster-secret",
        "Port": 5432,
        "BackupRetentionPeriod": 14,
        "DatabaseName": "appdb",
        "StorageEncrypted": true,
        "DeletionProtection": false
      }
    }
  },
  "Outputs": {
    "InstanceArn": {"Value": {"Fn::GetAtt": ["DbInstance", "DBInstanceArn"]}},
    "ClusterArn": {"Value": {"Fn::GetAtt": ["DbCluster", "DBClusterArn"]}}
  }
}"#;

#[tokio::test]
async fn cfn_provisions_rds_db_instance_and_cluster() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let rds = aws_sdk_rds::Client::new(&server.aws_config().await);

    cfn.create_stack()
        .stack_name("rds-db-stack")
        .template_body(TEMPLATE)
        .capabilities(Capability::CapabilityIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    // CreateStack provisions a container-backed DB instance in the
    // background, as AWS does, so wait for the stack to settle.
    let stack = helpers::wait_until(std::time::Duration::from_secs(300), || async {
        let out = cfn
            .describe_stacks()
            .stack_name("rds-db-stack")
            .send()
            .await
            .ok()?;
        let s = out.stacks().first()?.clone();
        (!s.stack_status()?.as_str().ends_with("IN_PROGRESS")).then_some(s)
    })
    .await
    .expect("stack reached a terminal status");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    // Verify DBInstance via DescribeDBInstances.
    let inst_resp = rds
        .describe_db_instances()
        .db_instance_identifier("cfn-instance")
        .send()
        .await
        .expect("describe_db_instances");
    let inst = inst_resp.db_instances().first().expect("instance present");
    assert_eq!(inst.db_instance_identifier(), Some("cfn-instance"));
    assert_eq!(inst.engine(), Some("postgres"));
    assert_eq!(inst.engine_version(), Some("16.0"));
    assert_eq!(inst.master_username(), Some("admin"));
    assert_eq!(inst.allocated_storage(), Some(20));
    assert_eq!(inst.multi_az(), Some(true));
    assert_eq!(inst.storage_encrypted(), Some(true));
    assert_eq!(inst.iam_database_authentication_enabled(), Some(true));
    assert_eq!(inst.backup_retention_period(), Some(7));

    // Verify DBCluster via DescribeDBClusters.
    let cluster_resp = rds
        .describe_db_clusters()
        .db_cluster_identifier("cfn-cluster")
        .send()
        .await
        .expect("describe_db_clusters");
    let cluster = cluster_resp.db_clusters().first().expect("cluster present");
    assert_eq!(cluster.db_cluster_identifier(), Some("cfn-cluster"));
    assert_eq!(cluster.engine(), Some("aurora-postgresql"));
    assert_eq!(cluster.master_username(), Some("admin"));
    assert_eq!(cluster.database_name(), Some("appdb"));
    assert_eq!(cluster.backup_retention_period(), Some(14));

    cfn.delete_stack()
        .stack_name("rds-db-stack")
        .send()
        .await
        .expect("delete_stack");
}
