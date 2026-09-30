//! State persisted by a China-region server before fakecloud minted ARNs in
//! the region's partition spells them `arn:aws:`. Restarting on such a data
//! directory with `--region cn-north-1` migrates them to `arn:aws-cn:` once,
//! so responses stop mixing partitions and lookups by the current ARN work.
//!
//! The legacy directory is produced for real: a commercial-region server
//! persists the resources (all ARNs `arn:aws:`), then the snapshots are moved
//! to `cn-north-1` textually and the version file loses its migration marker.
//! That is exactly what a pre-partition server running in `cn-north-1` wrote.

mod helpers;

use std::path::Path;

use aws_sdk_lambda::primitives::Blob;
use helpers::TestServer;
use md5::{Digest, Md5};

const ACCOUNT: &str = "123456789012";

fn make_python_zip() -> Vec<u8> {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file("index.py", zip::write::SimpleFileOptions::default())
        .unwrap();
    writer
        .write_all(b"def handler(event, context):\n    return {}\n")
        .unwrap();
    writer.finish().unwrap().into_inner()
}

async fn start(data_path: &Path, region: &str) -> TestServer {
    let data = data_path.display().to_string();
    TestServer::start_full(
        &[("FAKECLOUD_CONTAINER_CLI", "false")],
        &[
            "--storage-mode",
            "persistent",
            "--data-path",
            &data,
            "--region",
            region,
        ],
    )
    .await
}

/// Rewrite every persisted JSON/TOML state file from `us-east-1` to
/// `cn-north-1` and drop the version file's migration marker.
fn make_legacy_china_dir(dir: &Path) {
    fn walk(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path);
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            if !matches!(ext, Some("json" | "toml")) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let moved = text.replace("us-east-1", "cn-north-1");
            if moved != text {
                std::fs::write(&path, moved).unwrap();
            }
        }
    }
    walk(dir);
    let version_path = dir.join("fakecloud.version.toml");
    let version = std::fs::read_to_string(&version_path).unwrap();
    assert!(
        version.contains("arn_partitions_migrated = true"),
        "{version}"
    );
    let legacy: String = version
        .lines()
        .filter(|l| !l.starts_with("arn_partitions_migrated"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&version_path, legacy).unwrap();
}

#[tokio::test]
async fn china_server_migrates_legacy_aws_partition_arns() {
    let tmp = tempfile::tempdir().unwrap();

    // Phase 1: a commercial server persists resources with `arn:aws:` ARNs.
    let server = start(tmp.path(), "us-east-1").await;
    let config = server.aws_config_in("us-east-1").await;

    let kms = aws_sdk_kms::Client::new(&config);
    let key = kms.create_key().send().await.unwrap();
    let key_id = key.key_metadata().unwrap().key_id().to_string();
    kms.create_alias()
        .alias_name("alias/legacy")
        .target_key_id(&key_id)
        .send()
        .await
        .unwrap();

    let sns = aws_sdk_sns::Client::new(&config);
    sns.create_topic()
        .name("legacy-topic")
        .send()
        .await
        .unwrap();

    let iam = aws_sdk_iam::Client::new(&config);
    let trust = format!(
        r#"{{"Version":"2012-10-17","Statement":[{{"Effect":"Allow","Principal":{{"AWS":"arn:aws:iam::{ACCOUNT}:root","Service":"lambda.amazonaws.com"}},"Action":"sts:AssumeRole"}}]}}"#
    );
    iam.create_role()
        .role_name("legacy-role")
        .assume_role_policy_document(&trust)
        .send()
        .await
        .unwrap();
    let inline = format!(
        r#"{{"Version":"2012-10-17","Statement":[{{"Effect":"Allow","Action":"sqs:SendMessage","Resource":"arn:aws:sqs:us-east-1:{ACCOUNT}:legacy-queue"}}]}}"#
    );
    iam.put_role_policy()
        .role_name("legacy-role")
        .policy_name("send")
        .policy_document(&inline)
        .send()
        .await
        .unwrap();
    let customer_policy = iam
        .create_policy()
        .policy_name("legacy-policy")
        .policy_document(&inline)
        .send()
        .await
        .unwrap();
    let customer_policy_arn = customer_policy.policy().unwrap().arn().unwrap().to_string();
    iam.attach_role_policy()
        .role_name("legacy-role")
        .policy_arn(&customer_policy_arn)
        .send()
        .await
        .unwrap();
    iam.attach_role_policy()
        .role_name("legacy-role")
        .policy_arn("arn:aws:iam::aws:policy/ReadOnlyAccess")
        .send()
        .await
        .unwrap();

    let sqs = aws_sdk_sqs::Client::new(&config);
    sqs.create_queue()
        .queue_name("legacy-dlq")
        .send()
        .await
        .unwrap();
    let redrive = format!(
        r#"{{"deadLetterTargetArn":"arn:aws:sqs:us-east-1:{ACCOUNT}:legacy-dlq","maxReceiveCount":"3"}}"#
    );
    let queue_url = sqs
        .create_queue()
        .queue_name("legacy-queue")
        .attributes(
            aws_sdk_sqs::types::QueueAttributeName::RedrivePolicy,
            &redrive,
        )
        .attributes(
            aws_sdk_sqs::types::QueueAttributeName::SqsManagedSseEnabled,
            "false",
        )
        .send()
        .await
        .unwrap()
        .queue_url()
        .unwrap()
        .to_string();
    // A message whose body names a legacy China ARN: it is a payload verified
    // by its MD5, so the migration must leave it alone. (SSE is off so the
    // body is stored as plaintext, where the migration could see it.)
    let body = format!(r#"{{"TopicArn":"arn:aws:sns:cn-north-1:{ACCOUNT}:legacy-topic"}}"#);
    sqs.send_message()
        .queue_url(&queue_url)
        .message_body(&body)
        .send()
        .await
        .unwrap();

    let s3 = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::from(&config)
            .force_path_style(true)
            .build(),
    );
    s3.create_bucket()
        .bucket("legacy-bucket")
        .send()
        .await
        .unwrap();
    let bucket_policy = format!(
        r#"{{"Version":"2012-10-17","Statement":[{{"Effect":"Allow","Principal":{{"AWS":"arn:aws:iam::{ACCOUNT}:role/legacy-role"}},"Action":"s3:GetObject","Resource":"arn:aws:s3:::legacy-bucket/*"}}]}}"#
    );
    s3.put_bucket_policy()
        .bucket("legacy-bucket")
        .policy(&bucket_policy)
        .send()
        .await
        .unwrap();

    let lambda = aws_sdk_lambda::Client::new(&config);
    lambda
        .create_function()
        .function_name("legacy-fn")
        .runtime(aws_sdk_lambda::types::Runtime::Python312)
        .role(format!("arn:aws:iam::{ACCOUNT}:role/legacy-role"))
        .handler("index.handler")
        .code(
            aws_sdk_lambda::types::FunctionCode::builder()
                .zip_file(Blob::new(make_python_zip()))
                .build(),
        )
        .send()
        .await
        .unwrap();
    drop(server);

    make_legacy_china_dir(tmp.path());
    let sqs_snapshot = std::fs::read_to_string(tmp.path().join("sqs/snapshot.json")).unwrap();
    assert!(
        sqs_snapshot.contains(r#"\"TopicArn\":\"arn:aws:sns:cn-north-1"#),
        "the message body is stored in plaintext"
    );
    for service in ["kms", "sns", "iam", "sqs", "lambda"] {
        let snapshot =
            std::fs::read_to_string(tmp.path().join(service).join("snapshot.json")).unwrap();
        assert!(snapshot.contains("arn:aws:"), "{service} has no legacy ARN");
        assert!(
            !snapshot.contains("arn:aws-cn:"),
            "{service} already migrated"
        );
    }

    // Phase 2: the same directory served from cn-north-1.
    let server = start(tmp.path(), "cn-north-1").await;
    let config = server.aws_config_in("cn-north-1").await;

    // KMS: key and alias ARNs, lookups by the new ARN and by alias.
    let kms = aws_sdk_kms::Client::new(&config);
    let key_arn = format!("arn:aws-cn:kms:cn-north-1:{ACCOUNT}:key/{key_id}");
    let described = kms.describe_key().key_id(&key_id).send().await.unwrap();
    assert_eq!(
        described.key_metadata().unwrap().arn(),
        Some(key_arn.as_str())
    );
    let by_arn = kms.describe_key().key_id(&key_arn).send().await.unwrap();
    assert_eq!(by_arn.key_metadata().unwrap().key_id(), key_id);
    let by_alias = kms
        .describe_key()
        .key_id("alias/legacy")
        .send()
        .await
        .unwrap();
    assert_eq!(
        by_alias.key_metadata().unwrap().arn(),
        Some(key_arn.as_str())
    );
    let aliases = kms.list_aliases().send().await.unwrap();
    let alias = aliases
        .aliases()
        .iter()
        .find(|a| a.alias_name() == Some("alias/legacy"))
        .expect("legacy alias survives");
    assert_eq!(
        alias.alias_arn(),
        Some(format!("arn:aws-cn:kms:cn-north-1:{ACCOUNT}:alias/legacy").as_str())
    );

    // SNS: topics are keyed by ARN; the migrated key is what lookups hit, and
    // a topic created now lists alongside it in the same partition.
    let sns = aws_sdk_sns::Client::new(&config);
    let topic_arn = format!("arn:aws-cn:sns:cn-north-1:{ACCOUNT}:legacy-topic");
    sns.create_topic().name("new-topic").send().await.unwrap();
    let mut topics: Vec<String> = sns
        .list_topics()
        .send()
        .await
        .unwrap()
        .topics()
        .iter()
        .filter_map(|t| t.topic_arn().map(str::to_string))
        .collect();
    topics.sort();
    assert_eq!(
        topics,
        vec![
            topic_arn.clone(),
            format!("arn:aws-cn:sns:cn-north-1:{ACCOUNT}:new-topic"),
        ]
    );
    let attrs = sns
        .get_topic_attributes()
        .topic_arn(&topic_arn)
        .send()
        .await
        .unwrap();
    let attrs = attrs.attributes().unwrap();
    assert_eq!(attrs.get("TopicArn"), Some(&topic_arn));
    assert!(
        !attrs.values().any(|v| v.contains("arn:aws:")),
        "legacy ARN left in topic attributes: {attrs:?}"
    );

    // IAM: role, trust and inline policy documents, attached policies. The
    // AWS-managed policy keeps its `aws` spelling (fakecloud's catalog does).
    let iam = aws_sdk_iam::Client::new(&config);
    let role = iam
        .get_role()
        .role_name("legacy-role")
        .send()
        .await
        .unwrap();
    let role = role.role().unwrap();
    let role_arn = format!("arn:aws-cn:iam::{ACCOUNT}:role/legacy-role");
    assert_eq!(role.arn(), role_arn);
    let trust_doc = urlencoding_decode(role.assume_role_policy_document().unwrap());
    assert!(
        trust_doc.contains(&format!("arn:aws-cn:iam::{ACCOUNT}:root")),
        "{trust_doc}"
    );
    let inline_doc = iam
        .get_role_policy()
        .role_name("legacy-role")
        .policy_name("send")
        .send()
        .await
        .unwrap();
    let inline_doc = urlencoding_decode(inline_doc.policy_document());
    assert!(
        inline_doc.contains(&format!("arn:aws-cn:sqs:cn-north-1:{ACCOUNT}:legacy-queue")),
        "{inline_doc}"
    );
    let mut attached: Vec<String> = iam
        .list_attached_role_policies()
        .role_name("legacy-role")
        .send()
        .await
        .unwrap()
        .attached_policies()
        .iter()
        .filter_map(|p| p.policy_arn().map(str::to_string))
        .collect();
    attached.sort();
    let customer_arn = format!("arn:aws-cn:iam::{ACCOUNT}:policy/legacy-policy");
    assert_eq!(
        attached,
        vec![
            customer_arn.clone(),
            "arn:aws:iam::aws:policy/ReadOnlyAccess".to_string(),
        ]
    );
    let policy = iam
        .get_policy()
        .policy_arn(&customer_arn)
        .send()
        .await
        .unwrap();
    assert_eq!(policy.policy().unwrap().arn(), Some(customer_arn.as_str()));
    iam.get_policy()
        .policy_arn("arn:aws:iam::aws:policy/ReadOnlyAccess")
        .send()
        .await
        .expect("the managed policy still resolves");

    // SQS: queue and redrive ARNs migrate; the stored message body does not,
    // so its MD5 still matches.
    let sqs = aws_sdk_sqs::Client::new(&config);
    let queue_url = sqs
        .get_queue_url()
        .queue_name("legacy-queue")
        .send()
        .await
        .unwrap()
        .queue_url()
        .unwrap()
        .to_string();
    let attrs = sqs
        .get_queue_attributes()
        .queue_url(&queue_url)
        .attribute_names(aws_sdk_sqs::types::QueueAttributeName::All)
        .send()
        .await
        .unwrap();
    let attrs = attrs.attributes().unwrap();
    assert_eq!(
        attrs.get(&aws_sdk_sqs::types::QueueAttributeName::QueueArn),
        Some(&format!("arn:aws-cn:sqs:cn-north-1:{ACCOUNT}:legacy-queue"))
    );
    let redrive = attrs
        .get(&aws_sdk_sqs::types::QueueAttributeName::RedrivePolicy)
        .unwrap();
    assert!(
        redrive.contains(&format!("arn:aws-cn:sqs:cn-north-1:{ACCOUNT}:legacy-dlq")),
        "{redrive}"
    );
    let received = sqs
        .receive_message()
        .queue_url(&queue_url)
        .send()
        .await
        .unwrap();
    let message = &received.messages()[0];
    assert_eq!(message.body(), Some(body.as_str()));
    assert_eq!(
        message.md5_of_body(),
        Some(hex::encode(Md5::digest(body.as_bytes())).as_str())
    );

    // S3: the bucket policy (a stored document) names the bucket in aws-cn.
    let s3 = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::from(&config)
            .force_path_style(true)
            .build(),
    );
    let policy = s3
        .get_bucket_policy()
        .bucket("legacy-bucket")
        .send()
        .await
        .unwrap();
    let policy = policy.policy().unwrap();
    assert!(
        policy.contains("arn:aws-cn:s3:::legacy-bucket/*"),
        "{policy}"
    );
    assert!(
        policy.contains(&format!("arn:aws-cn:iam::{ACCOUNT}:role/legacy-role")),
        "{policy}"
    );
    assert!(!policy.contains("arn:aws:"), "{policy}");

    // Lambda: function and role ARNs; lookup by the new function ARN.
    let lambda = aws_sdk_lambda::Client::new(&config);
    let function_arn = format!("arn:aws-cn:lambda:cn-north-1:{ACCOUNT}:function:legacy-fn");
    let function = lambda
        .get_function()
        .function_name(&function_arn)
        .send()
        .await
        .unwrap();
    let configuration = function.configuration().unwrap();
    assert_eq!(configuration.function_arn(), Some(function_arn.as_str()));
    assert_eq!(configuration.role(), Some(role_arn.as_str()));

    // The directory is marked, so the migration never runs again.
    let version = std::fs::read_to_string(tmp.path().join("fakecloud.version.toml")).unwrap();
    assert!(
        version.contains("arn_partitions_migrated = true"),
        "{version}"
    );
}

/// IAM returns policy documents URL-encoded.
fn urlencoding_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            out.push(hex::decode(&s[i + 1..i + 3]).unwrap()[0]);
            i += 3;
        } else {
            out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}
