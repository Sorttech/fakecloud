use super::*;
use serde_json::json;

fn cn() -> ArnPartitionMigration {
    ArnPartitionMigration::for_server_region("cn-north-1")
}

fn commercial() -> ArnPartitionMigration {
    ArnPartitionMigration::for_server_region("us-east-1")
}

fn rewrite(m: &ArnPartitionMigration, s: &str) -> String {
    m.rewrite_str(s).into_owned()
}

#[test]
fn regional_arns_take_their_region_partition() {
    let m = commercial();
    assert_eq!(
        rewrite(&m, "arn:aws:kms:cn-north-1:123456789012:key/k"),
        "arn:aws-cn:kms:cn-north-1:123456789012:key/k"
    );
    assert_eq!(
        rewrite(&m, "arn:aws:sqs:us-gov-west-1:123456789012:q"),
        "arn:aws-us-gov:sqs:us-gov-west-1:123456789012:q"
    );
    assert_eq!(
        rewrite(&m, "arn:aws:sns:us-iso-east-1:123456789012:t"),
        "arn:aws-iso:sns:us-iso-east-1:123456789012:t"
    );
    assert_eq!(
        rewrite(&m, "arn:aws:sns:us-isob-east-1:123456789012:t"),
        "arn:aws-iso-b:sns:us-isob-east-1:123456789012:t"
    );
    assert_eq!(
        rewrite(&m, "arn:aws:lambda:us-isof-south-1:1:function:f"),
        "arn:aws-iso-f:lambda:us-isof-south-1:1:function:f"
    );
    assert_eq!(
        rewrite(&m, "arn:aws:lambda:eu-isoe-west-1:1:function:f"),
        "arn:aws-iso-e:lambda:eu-isoe-west-1:1:function:f"
    );
}

#[test]
fn commercial_region_arns_are_untouched() {
    for m in [cn(), commercial()] {
        let s = "arn:aws:kms:us-east-1:123456789012:key/k";
        assert!(matches!(m.rewrite_str(s), Cow::Borrowed(_)));
        assert!(!m.needs_rewrite(s.as_bytes()));
    }
}

#[test]
fn already_partitioned_arns_are_untouched_and_rewrite_is_idempotent() {
    let m = cn();
    for s in [
        "arn:aws-cn:kms:cn-north-1:123456789012:key/k",
        "arn:aws-cn:iam::123456789012:role/r",
        "arn:aws-us-gov:sqs:us-gov-west-1:1:q",
    ] {
        assert!(matches!(m.rewrite_str(s), Cow::Borrowed(_)), "{s}");
    }
    let once = rewrite(
        &m,
        "arn:aws:iam::123456789012:role/r arn:aws:sns:cn-north-1:1:t",
    );
    assert_eq!(rewrite(&m, &once), once);
}

#[test]
fn region_less_arns_follow_the_server_partition() {
    let m = cn();
    assert_eq!(
        rewrite(&m, "arn:aws:iam::123456789012:role/r"),
        "arn:aws-cn:iam::123456789012:role/r"
    );
    assert_eq!(
        rewrite(&m, "arn:aws:s3:::bucket/*"),
        "arn:aws-cn:s3:::bucket/*"
    );
    assert_eq!(
        rewrite(&m, "arn:aws:states:::lambda:invoke"),
        "arn:aws-cn:states:::lambda:invoke"
    );
    let gov = ArnPartitionMigration::for_server_region("us-gov-east-1");
    assert_eq!(
        rewrite(&gov, "arn:aws:iam::1:user/u"),
        "arn:aws-us-gov:iam::1:user/u"
    );

    // A commercial server keeps them: they are genuinely `aws`.
    let m = commercial();
    assert_eq!(
        rewrite(&m, "arn:aws:iam::123456789012:role/r"),
        "arn:aws:iam::123456789012:role/r"
    );
    assert!(!m.needs_rewrite(b"arn:aws:s3:::bucket"));
}

#[test]
fn aws_managed_policy_arns_keep_the_aws_partition() {
    let m = cn();
    for s in [
        "arn:aws:iam::aws:policy/AdministratorAccess",
        "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole",
    ] {
        assert_eq!(rewrite(&m, s), s);
        assert!(!m.needs_rewrite(s.as_bytes()));
    }
}

#[test]
fn free_text_and_malformed_tokens_are_untouched() {
    let m = cn();
    for s in [
        "arn:aws",
        "arn:aws:",
        "the arn:aws prefix",
        "arn:aws:kms",
        "arn:aws:kms:cn-north-1",
        "arn:aws:kms:cn-north-1:",
        "arn:aws:KMS:cn-north-1:1:key/k",
        "arn:aws::cn-north-1:1:key/k",
        "arn:aws:kms:cn north:1:key/k",
        "",
    ] {
        assert_eq!(rewrite(&m, s), s, "{s:?}");
        assert!(!m.needs_rewrite(s.as_bytes()), "{s:?}");
    }
    // Glued to a word: not a token. (The byte pre-check deliberately does
    // not look at the boundary; see the escape test.)
    for s in ["xarn:aws:kms:cn-north-1:1:key/k", "barn:aws:iam::1:role/r"] {
        assert_eq!(rewrite(&m, s), s, "{s:?}");
    }
}

#[test]
fn pre_check_sees_arns_behind_serialized_escapes() {
    // Regression: the raw bytes of `"a\narn:aws:..."` put `n` before the
    // token, which a boundary check on raw bytes rejected, so a file whose
    // only legacy ARN followed an escape was skipped and marked migrated.
    let m = cn();
    let value = json!({"doc": "line\narn:aws:iam::1:role/r\tarn:aws:sns:cn-north-1:1:t"});
    let raw = serde_json::to_vec(&value).unwrap();
    assert!(m.needs_rewrite(&raw));
    let mut value = value;
    assert!(m.rewrite_json(&mut value, &[]));
    assert_eq!(
        value["doc"],
        "line\narn:aws-cn:iam::1:role/r\tarn:aws-cn:sns:cn-north-1:1:t"
    );
}

#[test]
fn policy_wildcards_move_with_the_resources_they_match() {
    // Regression: `*` in the region or account field made the token
    // unparseable, so patterns stayed `arn:aws:` while the resources they
    // match became `arn:aws-cn:` and stored allows stopped matching.
    let m = cn();
    for (legacy, migrated) in [
        (
            "arn:aws:kms:*:123456789012:key/*",
            "arn:aws-cn:kms:*:123456789012:key/*",
        ),
        (
            "arn:aws:sqs:cn-north-1:*:q",
            "arn:aws-cn:sqs:cn-north-1:*:q",
        ),
        ("arn:aws:sqs:cn-*:1:q", "arn:aws-cn:sqs:cn-*:1:q"),
        ("arn:aws:iam::*:role/x", "arn:aws-cn:iam::*:role/x"),
        ("arn:aws:s3:::*", "arn:aws-cn:s3:::*"),
        ("arn:aws:*:*:*:*", "arn:aws-cn:*:*:*:*"),
        (
            "arn:aws:ec2:cn-north-?:1:vpc/*",
            "arn:aws-cn:ec2:cn-north-?:1:vpc/*",
        ),
    ] {
        assert_eq!(rewrite(&m, legacy), migrated, "{legacy}");
    }
    // A pattern that pins a partition keeps doing so on any server.
    let us = commercial();
    assert_eq!(
        rewrite(&us, "arn:aws:sqs:cn-*:1:q"),
        "arn:aws-cn:sqs:cn-*:1:q"
    );
    // A partition-agnostic pattern on a commercial server is genuinely aws.
    assert_eq!(
        rewrite(&us, "arn:aws:kms:*:1:key/*"),
        "arn:aws:kms:*:1:key/*"
    );
    // Any other partition-agnostic pattern follows the server's partition.
    assert_eq!(
        rewrite(&m, "arn:aws:sqs:us-*:1:q"),
        "arn:aws-cn:sqs:us-*:1:q"
    );
}

#[test]
fn embedded_arns_are_rewritten_in_place() {
    let m = cn();
    // A policy document stored as a string keeps its exact formatting.
    let policy = r#"{
  "Version": "2012-10-17",
  "Statement": [{"Effect":"Allow","Principal":{"AWS":"arn:aws:iam::123456789012:root"},
    "Action":"sqs:*","Resource":["arn:aws:sqs:cn-north-1:123456789012:q","arn:aws:sqs:us-east-1:1:other"]}]
}"#;
    let expected = policy
        .replace("arn:aws:iam::", "arn:aws-cn:iam::")
        .replace("arn:aws:sqs:cn-north-1", "arn:aws-cn:sqs:cn-north-1");
    assert_eq!(rewrite(&m, policy), expected);
    assert!(m.needs_rewrite(policy.as_bytes()));

    // Composite keys and delimited lists.
    assert_eq!(
        rewrite(
            &m,
            "123456789012|arn:aws:sns:cn-north-1:1:t,arn:aws:sns:cn-north-1:1:u"
        ),
        "123456789012|arn:aws-cn:sns:cn-north-1:1:t,arn:aws-cn:sns:cn-north-1:1:u"
    );
    // A token at the very end of the string.
    assert_eq!(rewrite(&m, "see arn:aws:s3:::b"), "see arn:aws-cn:s3:::b");
    // Non-ASCII text around a token.
    assert_eq!(
        rewrite(&m, "é arn:aws:sns:cn-north-1:1:t ü"),
        "é arn:aws-cn:sns:cn-north-1:1:t ü"
    );
}

#[test]
fn json_values_keys_and_nesting_are_rewritten() {
    let m = cn();
    let mut v = json!({
        "accounts": {
            "123456789012": {
                "topics": {
                    "arn:aws:sns:cn-north-1:123456789012:t": {
                        "topic_arn": "arn:aws:sns:cn-north-1:123456789012:t",
                        "attributes": {
                            "Policy": "{\"Resource\":\"arn:aws:sns:cn-north-1:123456789012:t\"}"
                        },
                        "count": 3,
                        "flags": [true, null, "arn:aws:iam::123456789012:role/r"]
                    }
                },
                "managed": ["arn:aws:iam::aws:policy/ReadOnlyAccess"],
            }
        }
    });
    assert!(m.rewrite_json(&mut v, &[]));
    assert_eq!(
        v,
        json!({
            "accounts": {
                "123456789012": {
                    "topics": {
                        "arn:aws-cn:sns:cn-north-1:123456789012:t": {
                            "topic_arn": "arn:aws-cn:sns:cn-north-1:123456789012:t",
                            "attributes": {
                                "Policy": "{\"Resource\":\"arn:aws-cn:sns:cn-north-1:123456789012:t\"}"
                            },
                            "count": 3,
                            "flags": [true, null, "arn:aws-cn:iam::123456789012:role/r"]
                        }
                    },
                    "managed": ["arn:aws:iam::aws:policy/ReadOnlyAccess"],
                }
            }
        })
    );
    // Idempotent.
    assert!(!m.rewrite_json(&mut v, &[]));
}

#[test]
fn opaque_keys_are_kept_verbatim() {
    let m = cn();
    let body = "{\"TopicArn\":\"arn:aws:sns:cn-north-1:1:t\"}";
    let mut v = json!({
        "queue_arn": "arn:aws:sqs:cn-north-1:1:q",
        "messages": [{"body": body, "md5_of_body": "x"}]
    });
    assert!(m.rewrite_json(&mut v, opaque_keys_for("sqs")));
    assert_eq!(v["queue_arn"], "arn:aws-cn:sqs:cn-north-1:1:q");
    assert_eq!(v["messages"][0]["body"], body);
}

#[test]
fn legacy_key_colliding_with_a_current_key_is_dropped() {
    let m = cn();
    for (first, second) in [
        (
            "arn:aws:sns:cn-north-1:1:t",
            "arn:aws-cn:sns:cn-north-1:1:t",
        ),
        (
            "arn:aws-cn:sns:cn-north-1:1:t",
            "arn:aws:sns:cn-north-1:1:t",
        ),
    ] {
        let mut map = Map::new();
        map.insert(
            first.to_string(),
            json!(if first.contains("aws-cn") {
                "current"
            } else {
                "legacy"
            }),
        );
        map.insert(
            second.to_string(),
            json!(if second.contains("aws-cn") {
                "current"
            } else {
                "legacy"
            }),
        );
        let mut v = Value::Object(map);
        assert!(m.rewrite_json(&mut v, &[]));
        assert_eq!(v, json!({"arn:aws-cn:sns:cn-north-1:1:t": "current"}));
    }
}

fn legacy_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join(crate::version::VERSION_FILE_NAME),
        "format_version = 1\nfakecloud_version = \"0.40.0\"\ncreated_at = \"2026-01-01T00:00:00Z\"\n",
    )
    .unwrap();
    tmp
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn migrates_a_legacy_data_dir_once() {
    let tmp = legacy_dir();
    let dir = tmp.path();
    let kms = dir.join("kms/snapshot.json");
    write(
        &kms,
        r#"{"schema_version":3,"accounts":{"k":{"arn":"arn:aws:kms:cn-north-1:1:key/k"}}}"#,
    );
    let sqs = dir.join("sqs/snapshot.json");
    write(
        &sqs,
        r#"{"q":{"arn":"arn:aws:sqs:cn-north-1:1:q","messages":[{"body":"arn:aws:sqs:cn-north-1:1:q"}]}}"#,
    );
    let logs_manifest = dir.join("logs/manifest.json");
    write(
        &logs_manifest,
        r#"{"group_arn":"arn:aws:logs:cn-north-1:1:log-group:g"}"#,
    );
    let logs_segment = dir.join("logs/0f8fad5b-d9cb-469f-a165-70867728950e.jsonl");
    let segment = "{\"message\":\"arn:aws:logs:cn-north-1:1:log-group:g\"}\n";
    write(&logs_segment, segment);
    let untouched = dir.join("ssm/snapshot.json");
    let untouched_text = r#"{"p":"arn:aws:ssm:us-east-1:1:parameter/p"}"#;
    write(&untouched, untouched_text);
    let policy = dir.join("s3/buckets/b/policy.toml");
    // Subresource files hold the configuration payload verbatim.
    write(&policy, r#"{"Resource":"arn:aws:s3:::b/*"}"#);
    let object_meta = dir.join("s3/buckets/b/objects/k/null.toml");
    write(
        &object_meta,
        "sse_kms_key_id = \"arn:aws:kms:cn-north-1:1:key/k\"\n",
    );
    let body = dir.join("s3/buckets/b/objects/k/null.bin");
    write(&body, "arn:aws:kms:cn-north-1:1:key/k");
    let broken = dir.join("sns/snapshot.json");
    write(&broken, "{\"arn:aws:sns:cn-north-1:1:t\"");

    let report = migrate_data_dir(dir, "cn-north-1").unwrap();
    assert!(!report.already_migrated);
    assert_eq!(report.skipped, vec![broken.clone()]);
    let mut rewritten = report.rewritten.clone();
    rewritten.sort();
    let mut expected = vec![
        kms.clone(),
        sqs.clone(),
        logs_manifest.clone(),
        policy.clone(),
        object_meta.clone(),
    ];
    expected.sort();
    assert_eq!(rewritten, expected);

    let kms_v: Value = serde_json::from_str(&read(&kms)).unwrap();
    assert_eq!(
        kms_v["accounts"]["k"]["arn"],
        "arn:aws-cn:kms:cn-north-1:1:key/k"
    );
    assert_eq!(kms_v["schema_version"], 3);
    let sqs_v: Value = serde_json::from_str(&read(&sqs)).unwrap();
    assert_eq!(sqs_v["q"]["arn"], "arn:aws-cn:sqs:cn-north-1:1:q");
    assert_eq!(
        sqs_v["q"]["messages"][0]["body"],
        "arn:aws:sqs:cn-north-1:1:q"
    );
    assert!(read(&logs_manifest).contains("arn:aws-cn:logs:cn-north-1"));
    assert_eq!(read(&logs_segment), segment);
    assert_eq!(read(&untouched), untouched_text);
    assert_eq!(read(&policy), r#"{"Resource":"arn:aws-cn:s3:::b/*"}"#);
    assert!(read(&object_meta).contains("arn:aws-cn:kms:cn-north-1"));
    assert_eq!(read(&body), "arn:aws:kms:cn-north-1:1:key/k");
    assert_eq!(read(&broken), "{\"arn:aws:sns:cn-north-1:1:t\"");

    // Marked: the version file keeps its fields and gains the marker.
    let version: crate::version::FormatVersion =
        toml::from_str(&read(&dir.join(crate::version::VERSION_FILE_NAME))).unwrap();
    assert!(version.arn_partitions_migrated);
    assert_eq!(version.fakecloud_version, "0.40.0");
    assert_eq!(version.created_at, "2026-01-01T00:00:00Z");

    // A second start reads nothing, even with new legacy-looking state.
    let later = r#"{"arn":"arn:aws:iam::1:role/created-in-us-east-1"}"#;
    write(&dir.join("iam/snapshot.json"), later);
    let again = migrate_data_dir(dir, "cn-north-1").unwrap();
    assert!(again.already_migrated);
    assert_eq!(read(&dir.join("iam/snapshot.json")), later);
}

#[test]
fn commercial_server_rewrites_only_regional_arns() {
    let tmp = legacy_dir();
    let dir = tmp.path();
    let iam = dir.join("iam/snapshot.json");
    let iam_text = r#"{"arn":"arn:aws:iam::1:role/r"}"#;
    write(&iam, iam_text);
    let kms = dir.join("kms/snapshot.json");
    write(&kms, r#"{"arn":"arn:aws:kms:cn-north-1:1:key/k"}"#);

    let report = migrate_data_dir(dir, "us-east-1").unwrap();
    assert_eq!(report.rewritten, vec![kms.clone()]);
    assert_eq!(read(&iam), iam_text);
    assert!(read(&kms).contains("arn:aws-cn:kms:cn-north-1"));
    assert!(crate::version::arn_partitions_migrated(dir).unwrap());
}

#[test]
fn fresh_data_dir_is_born_migrated() {
    let tmp = tempfile::tempdir().unwrap();
    crate::version::ensure_version_file(tmp.path(), "test").unwrap();
    assert!(crate::version::arn_partitions_migrated(tmp.path()).unwrap());
    let report = migrate_data_dir(tmp.path(), "cn-north-1").unwrap();
    assert!(report.already_migrated);
}

#[test]
fn legacy_version_file_without_marker_is_accepted_by_the_version_check() {
    let tmp = legacy_dir();
    crate::version::check_version_file(tmp.path()).unwrap();
    assert!(!crate::version::arn_partitions_migrated(tmp.path()).unwrap());
}

#[test]
fn customer_data_is_kept_verbatim() {
    // Regression: DynamoDB rows, SSM values and secrets were rewritten, so an
    // item keyed by an ARN string was no longer reachable by the key the
    // application wrote, and reads returned values nobody stored.
    let tmp = legacy_dir();
    let dir = tmp.path();
    let role = "arn:aws:iam::123456789012:role/app";
    let ddb = dir.join("dynamodb/snapshot.json");
    write(
        &ddb,
        &json!({
            "table": {
                "arn": "arn:aws:dynamodb:cn-north-1:1:table/t",
                "items": [{"pk": {"S": role}}],
                "stream_records": [{"keys": {"pk": {"S": role}}, "new_image": {"pk": {"S": role}}, "old_image": null}],
                "backups": [{"items": [{"pk": {"S": role}}]}]
            }
        })
        .to_string(),
    );
    let ssm = dir.join("ssm/snapshot.json");
    write(
        &ssm,
        &json!({"p": {"arn": "arn:aws:ssm:cn-north-1:1:parameter/p", "value": role, "history": [{"value": role}]}})
            .to_string(),
    );
    let secrets = dir.join("secretsmanager/snapshot.json");
    write(
        &secrets,
        &json!({"s": {"arn": "arn:aws:secretsmanager:cn-north-1:1:secret:s", "secret_string": role}})
            .to_string(),
    );

    migrate_data_dir(dir, "cn-north-1").unwrap();

    let ddb: Value = serde_json::from_str(&read(&ddb)).unwrap();
    assert_eq!(
        ddb["table"]["arn"],
        "arn:aws-cn:dynamodb:cn-north-1:1:table/t"
    );
    assert_eq!(ddb["table"]["items"][0]["pk"]["S"], role);
    assert_eq!(ddb["table"]["stream_records"][0]["keys"]["pk"]["S"], role);
    assert_eq!(
        ddb["table"]["stream_records"][0]["new_image"]["pk"]["S"],
        role
    );
    assert_eq!(ddb["table"]["backups"][0]["items"][0]["pk"]["S"], role);
    let ssm: Value = serde_json::from_str(&read(&ssm)).unwrap();
    assert_eq!(ssm["p"]["arn"], "arn:aws-cn:ssm:cn-north-1:1:parameter/p");
    assert_eq!(ssm["p"]["value"], role);
    assert_eq!(ssm["p"]["history"][0]["value"], role);
    let secrets: Value = serde_json::from_str(&read(&secrets)).unwrap();
    assert_eq!(
        secrets["s"]["arn"],
        "arn:aws-cn:secretsmanager:cn-north-1:1:secret:s"
    );
    assert_eq!(secrets["s"]["secret_string"], role);
}

#[test]
fn s3_object_keys_metadata_and_tags_are_kept_verbatim() {
    // Regression: rewriting the sidecar's `key` moved the object to a new key
    // while its files stayed in the directory derived from the old one.
    let tmp = legacy_dir();
    let dir = tmp.path();
    let object = dir.join("s3/buckets/b/objects/escaped-key/null.toml");
    write(
        &object,
        "key = \"exports/arn:aws:iam::1:role/x.json\"\n\
         sse_kms_key_id = \"arn:aws:kms:cn-north-1:1:key/k\"\n\
         last_modified = \"2026-01-01T00:00:00Z\"\n\
         part_sizes = [[1, 5]]\n\n\
         [metadata]\nsource = \"arn:aws:sns:cn-north-1:1:t\"\n\n\
         [tags]\nowner = \"arn:aws:iam::1:role/x\"\n",
    );
    let upload = dir.join("s3/buckets/b/mpu/u1/init.toml");
    write(
        &upload,
        "key = \"arn:aws:iam::1:role/x\"\nsse_kms_key_id = \"arn:aws:kms:cn-north-1:1:key/k\"\n",
    );
    let replication = dir.join("s3/buckets/b/replication.toml");
    write(
        &replication,
        "<ReplicationConfiguration><Role>arn:aws:iam::1:role/replicator</Role>\
         <Rule><Destination><Bucket>arn:aws:s3:::dest</Bucket></Destination></Rule>\
         </ReplicationConfiguration>",
    );
    let meta = dir.join("s3/buckets/b/meta.toml");
    let meta_text = "name = \"b\"\nregion = \"cn-north-1\"\n";
    write(&meta, meta_text);

    migrate_data_dir(dir, "cn-north-1").unwrap();

    let object: toml::Table = read(&object).parse().unwrap();
    assert_eq!(
        object["key"].as_str(),
        Some("exports/arn:aws:iam::1:role/x.json")
    );
    assert_eq!(
        object["sse_kms_key_id"].as_str(),
        Some("arn:aws-cn:kms:cn-north-1:1:key/k")
    );
    assert_eq!(
        object["last_modified"].as_str(),
        Some("2026-01-01T00:00:00Z")
    );
    assert_eq!(
        object["part_sizes"],
        toml::Value::Array(vec![toml::Value::Array(vec![1.into(), 5.into()])])
    );
    assert_eq!(
        object["metadata"]["source"].as_str(),
        Some("arn:aws:sns:cn-north-1:1:t")
    );
    assert_eq!(
        object["tags"]["owner"].as_str(),
        Some("arn:aws:iam::1:role/x")
    );
    let upload: toml::Table = read(&upload).parse().unwrap();
    assert_eq!(upload["key"].as_str(), Some("arn:aws:iam::1:role/x"));
    assert_eq!(
        upload["sse_kms_key_id"].as_str(),
        Some("arn:aws-cn:kms:cn-north-1:1:key/k")
    );
    assert_eq!(
        read(&replication),
        "<ReplicationConfiguration><Role>arn:aws-cn:iam::1:role/replicator</Role>\
         <Rule><Destination><Bucket>arn:aws-cn:s3:::dest</Bucket></Destination></Rule>\
         </ReplicationConfiguration>"
    );
    assert_eq!(read(&meta), meta_text);
}

#[cfg(unix)]
#[test]
fn unreadable_and_filesystem_artifact_directories_do_not_stop_startup() {
    // Regression: `read_dir` on a root-only `lost+found` at the root of a
    // volume failed the migration, and with it the server's first start.
    use std::os::unix::fs::PermissionsExt;
    let tmp = legacy_dir();
    let dir = tmp.path();
    let kms = dir.join("kms/snapshot.json");
    write(&kms, r#"{"arn":"arn:aws:kms:cn-north-1:1:key/k"}"#);
    let locked = [dir.join("lost+found"), dir.join("elasticache")];
    for path in &locked {
        std::fs::create_dir(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    // Running as root ignores the mode; nothing to prove then.
    let enforced = std::fs::read_dir(&locked[1]).is_err();

    let report = migrate_data_dir(dir, "cn-north-1");
    for path in &locked {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let report = report.unwrap();
    assert_eq!(report.rewritten, vec![kms.clone()]);
    if enforced {
        // `lost+found` is never even opened; a service directory the server
        // cannot read is reported and left to its loader.
        assert_eq!(report.skipped, vec![locked[1].clone()]);
    }
    assert!(crate::version::arn_partitions_migrated(dir).unwrap());
}
