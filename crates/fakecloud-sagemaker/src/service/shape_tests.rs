//! Output-shape fidelity, driven by the Smithy model itself
//! (`aws-models/sagemaker.json`), independently of the generated tables the
//! service projects with: every resource family is created from a request
//! that sets every member the model allows (so request-only members and
//! input-shaped nested structures are stored), then every Describe / Get /
//! List operation's response is validated against the model's output shape.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use super::tests::{resp_json, run, svc};

const NS: &str = "com.amazonaws.sagemaker#";

struct Model {
    shapes: Map<String, Value>,
}

impl Model {
    fn load() -> Self {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../aws-models/sagemaker.json"
        );
        let doc: Value =
            serde_json::from_slice(&std::fs::read(path).expect("read sagemaker model"))
                .expect("parse sagemaker model");
        Model {
            shapes: doc["shapes"].as_object().expect("shapes").clone(),
        }
    }

    fn shape(&self, id: &str) -> Option<&Value> {
        self.shapes.get(id)
    }

    /// The shape's type; prelude (`smithy.api#...`) shapes map to their
    /// lower-case simple type.
    fn ty(&self, id: &str) -> String {
        match self.shape(id) {
            Some(s) => s["type"].as_str().unwrap_or("string").to_string(),
            None => id.rsplit('#').next().unwrap_or("String").to_lowercase(),
        }
    }

    fn members<'a>(&'a self, id: &str) -> Vec<(&'a str, &'a Value)> {
        self.shape(id)
            .and_then(|s| s["members"].as_object())
            .map(|m| m.iter().map(|(k, v)| (k.as_str(), v)).collect())
            .unwrap_or_default()
    }

    fn operation_io(&self, op: &str, which: &str) -> Option<String> {
        let t = self.shape(&format!("{NS}{op}"))?[which]["target"].as_str()?;
        (t != "smithy.api#Unit").then(|| t.to_string())
    }

    fn operations(&self) -> Vec<String> {
        let mut ops: Vec<String> = self
            .shapes
            .iter()
            .filter(|(_, v)| v["type"] == "operation")
            .map(|(k, _)| k.trim_start_matches(NS).to_string())
            .collect();
        ops.sort();
        ops
    }

    fn enum_values(&self, id: &str) -> Vec<Value> {
        self.members(id)
            .into_iter()
            .map(|(name, m)| match &m["traits"]["smithy.api#enumValue"] {
                Value::Null => Value::String(name.to_string()),
                v => v.clone(),
            })
            .collect()
    }

    fn traits<'a>(&'a self, id: &str) -> Option<&'a Value> {
        self.shape(id).map(|s| &s["traits"])
    }

    /// A value for every member the model allows: optional members too (to a
    /// bounded depth), one element per list / map, the first union member.
    /// Strings carry the member name so a resource's identifier comes out the
    /// same in its create and its describe.
    fn sample(&self, id: &str, member: &str, depth: usize) -> Value {
        let ty = self.ty(id);
        let traits = self.traits(id).cloned().unwrap_or(Value::Null);
        match ty.as_str() {
            "structure" | "union" => {
                let mut m = Map::new();
                for (name, mm) in self.members(id) {
                    let required = mm["traits"].get("smithy.api#required").is_some();
                    if ty == "union" && !m.is_empty() {
                        break;
                    }
                    // A union always carries one member.
                    if required || ty == "union" || depth < 4 {
                        m.insert(
                            name.to_string(),
                            self.sample(mm["target"].as_str().unwrap(), name, depth + 1),
                        );
                    }
                }
                Value::Object(m)
            }
            "list" => {
                let elem = self.shape(id).unwrap()["member"]["target"]
                    .as_str()
                    .unwrap();
                Value::Array(vec![self.sample(elem, member, depth + 1)])
            }
            "map" => {
                let s = self.shape(id).unwrap();
                let key = self.sample(s["key"]["target"].as_str().unwrap(), "key", depth + 1);
                let mut m = Map::new();
                m.insert(
                    key.as_str().unwrap_or("key").to_string(),
                    self.sample(s["value"]["target"].as_str().unwrap(), member, depth + 1),
                );
                Value::Object(m)
            }
            "enum" | "intenum" | "intEnum" => self.enum_values(id).swap_remove(0),
            "string" => {
                let len = &traits["smithy.api#length"];
                let min = len["min"].as_u64().unwrap_or(1).max(1) as usize;
                let max = len["max"].as_u64().unwrap_or(256) as usize;
                let mut s = member.to_string();
                while s.len() < min {
                    s.push('a');
                }
                s.truncate(max.max(1));
                Value::String(s)
            }
            "integer" | "long" | "short" | "byte" | "biginteger" => {
                let r = &traits["smithy.api#range"];
                let min = r["min"].as_f64().unwrap_or(1.0).max(1.0);
                let max = r["max"].as_f64().unwrap_or(f64::MAX);
                Value::from(min.min(max) as i64)
            }
            "float" | "double" | "bigdecimal" => {
                let r = &traits["smithy.api#range"];
                let min = r["min"].as_f64().unwrap_or(1.0).max(1.0);
                let max = r["max"].as_f64().unwrap_or(f64::MAX);
                Value::from(min.min(max))
            }
            "boolean" => Value::Bool(true),
            "timestamp" => Value::from(1_752_324_947.0),
            "blob" => Value::String("YmxvYg==".to_string()),
            _ => Value::String(member.to_string()),
        }
    }

    /// Every way `v` departs from shape `id`: undeclared or missing required
    /// members, wrong JSON types, values outside an enum, malformed unions.
    fn violations(&self, id: &str, v: &Value, path: &str, out: &mut Vec<String>) {
        let ty = self.ty(id);
        let ok = match ty.as_str() {
            "structure" | "union" => {
                let Some(obj) = v.as_object() else {
                    out.push(format!("{path}: expected an object, got {v}"));
                    return;
                };
                let members: BTreeMap<&str, &Value> = self.members(id).into_iter().collect();
                for (k, child) in obj {
                    match members.get(k.as_str()) {
                        None => out.push(format!("{path}.{k}: not a member of {id}")),
                        Some(m) => self.violations(
                            m["target"].as_str().unwrap(),
                            child,
                            &format!("{path}.{k}"),
                            out,
                        ),
                    }
                }
                if ty == "union" {
                    if obj.len() != 1 {
                        out.push(format!("{path}: union {id} carries {} members", obj.len()));
                    }
                } else {
                    for (name, m) in &members {
                        if m["traits"].get("smithy.api#required").is_some()
                            && obj.get(*name).is_none_or(Value::is_null)
                        {
                            out.push(format!("{path}.{name}: required member missing"));
                        }
                    }
                }
                true
            }
            "list" => match v.as_array() {
                Some(arr) => {
                    let elem = self.shape(id).unwrap()["member"]["target"]
                        .as_str()
                        .unwrap();
                    for (i, e) in arr.iter().enumerate() {
                        self.violations(elem, e, &format!("{path}[{i}]"), out);
                    }
                    true
                }
                None => false,
            },
            "map" => match v.as_object() {
                Some(obj) => {
                    let value = self.shape(id).unwrap()["value"]["target"].as_str().unwrap();
                    for (k, e) in obj {
                        self.violations(value, e, &format!("{path}{{{k}}}"), out);
                    }
                    true
                }
                None => false,
            },
            "enum" => {
                let values = self.enum_values(id);
                if v.is_string() && !values.contains(v) {
                    out.push(format!("{path}: {v} is not a value of {id}"));
                }
                v.is_string()
            }
            "intEnum" | "integer" | "long" | "short" | "byte" | "biginteger" => {
                v.is_i64() || v.is_u64()
            }
            "float" | "double" | "bigdecimal" => v.is_number(),
            "boolean" => v.is_boolean(),
            "timestamp" => v.is_number(),
            _ => v.is_string(),
        };
        if !ok {
            out.push(format!("{path}: {v} is not a {ty} ({id})"));
        }
    }
}

/// The resource family an operation name addresses (`DescribeCluster` ->
/// `Cluster`, `ListClusters` -> `Cluster`).
fn family(op: &str) -> Option<String> {
    for verb in ["Create", "Describe", "Get", "Update"] {
        if let Some(base) = op.strip_prefix(verb) {
            return Some(base.to_string());
        }
    }
    let base = op.strip_prefix("List")?;
    Some(if let Some(b) = base.strip_suffix("ies") {
        format!("{b}y")
    } else if let Some(b) = base.strip_suffix("sses") {
        format!("{b}ss")
    } else if base.ends_with('s') && !base.ends_with("ss") {
        base[..base.len() - 1].to_string()
    } else {
        base.to_string()
    })
}

#[test]
fn every_read_output_matches_its_model_output_shape() {
    let model = Model::load();
    let s = svc();
    let ops = model.operations();
    let mut violations = Vec::new();
    let check = |op: &str, resp: &Value, violations: &mut Vec<String>| {
        if let Some(out) = model.operation_io(op, "output") {
            model.violations(&out, resp, op, violations);
        }
    };

    // Create every family from a maximal request, then update it the same
    // way, so the stored records carry every write-shape member.
    let mut created = Vec::new();
    for verb in ["Create", "Update"] {
        for op in ops.iter().filter(|o| o.starts_with(verb)) {
            let Some(input) = model.operation_io(op, "input") else {
                continue;
            };
            let body = model.sample(&input, "", 0);
            if let Ok(resp) = run(&s, op, body) {
                if resp.status.is_success() {
                    check(op, &resp_json(&resp), &mut violations);
                    if verb == "Create" {
                        created.extend(family(op));
                    }
                }
            }
        }
    }

    // Read every family back.
    let mut validated = BTreeMap::new();
    for op in ops
        .iter()
        .filter(|o| o.starts_with("Describe") || o.starts_with("Get") || o.starts_with("List"))
    {
        // A List request carries only its required members: optional ones
        // are filters that would hide the stored resources.
        let body = model
            .operation_io(op, "input")
            .map(|i| {
                let full = model.sample(&i, "", 0);
                if !op.starts_with("List") {
                    return full;
                }
                let required: Map<String, Value> = full
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(k, _)| {
                        model.members(&i).iter().any(|(name, m)| {
                            name == k && m["traits"].get("smithy.api#required").is_some()
                        })
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Value::Object(required)
            })
            .unwrap_or(Value::Null);
        let Ok(resp) = run(&s, op, body) else {
            continue;
        };
        if !resp.status.is_success() {
            continue;
        }
        let got = resp_json(&resp);
        check(op, &got, &mut violations);
        let is_stored = family(op).is_some_and(|f| created.contains(&f));
        validated.insert(op.clone(), is_stored);
    }

    assert!(
        violations.is_empty(),
        "{} output-shape violations:\n{}",
        violations.len(),
        violations.join("\n")
    );
    // The sweep must actually have read stored resources back, not just
    // synthesised responses for unknown ones.
    let stored_reads = validated.values().filter(|s| **s).count();
    assert!(
        stored_reads >= 120,
        "only {stored_reads} reads of created resources validated: {validated:?}"
    );
    for op in [
        "DescribeCluster",
        "DescribeHumanTaskUi",
        "DescribeInferenceComponent",
        "DescribeWorkforce",
        "DescribeWorkteam",
        "ListWorkforces",
        "ListWorkteams",
    ] {
        assert_eq!(
            validated.get(op),
            Some(&true),
            "{op} was not validated against a stored resource"
        );
    }
}

#[test]
fn list_workforces_never_returns_the_oidc_client_secret() {
    let s = svc();
    run(
        &s,
        "CreateWorkforce",
        json!({"WorkforceName": "wf", "OidcConfig": {
            "ClientId": "cid", "ClientSecret": "s3cret", "Issuer": "https://i",
            "AuthorizationEndpoint": "https://a", "TokenEndpoint": "https://t",
            "UserInfoEndpoint": "https://u", "LogoutEndpoint": "https://l",
            "JwksUri": "https://j"
        }, "WorkforceVpcConfig": {"VpcId": "vpc-1", "SecurityGroupIds": ["sg-1"], "Subnets": ["subnet-1"]}}),
    )
    .unwrap();
    let listed = resp_json(&run(&s, "ListWorkforces", json!({})).unwrap());
    let wf = &listed["Workforces"][0];
    assert_eq!(wf["OidcConfig"]["ClientId"], "cid");
    assert!(wf["OidcConfig"].get("ClientSecret").is_none(), "{wf}");
    assert_eq!(wf["Status"], "Active");
    assert!(wf["CreateDate"].is_number(), "{wf}");
    assert_eq!(wf["WorkforceVpcConfig"]["VpcId"], "vpc-1");
    assert_eq!(wf["WorkforceVpcConfig"]["Subnets"], json!(["subnet-1"]));
    assert_eq!(
        wf["WorkforceVpcConfig"]["SecurityGroupIds"],
        json!(["sg-1"])
    );

    let described =
        resp_json(&run(&s, "DescribeWorkforce", json!({"WorkforceName": "wf"})).unwrap());
    let wf = &described["Workforce"];
    assert_eq!(wf["WorkforceName"], "wf");
    assert_eq!(wf["OidcConfig"]["Issuer"], "https://i");
    assert!(wf["OidcConfig"].get("ClientSecret").is_none(), "{wf}");
    assert!(described.get("OidcConfig").is_none(), "{described}");
}

#[test]
fn describe_workteam_reports_the_stored_team() {
    let s = svc();
    run(
        &s,
        "CreateWorkteam",
        json!({"WorkteamName": "team", "Description": "labelers", "MemberDefinitions": [
            {"CognitoMemberDefinition": {"UserPool": "p", "UserGroup": "g", "ClientId": "c"}}
        ]}),
    )
    .unwrap();
    let got = resp_json(&run(&s, "DescribeWorkteam", json!({"WorkteamName": "team"})).unwrap());
    let team = &got["Workteam"];
    assert_eq!(team["Description"], "labelers");
    assert_eq!(
        team["MemberDefinitions"][0]["CognitoMemberDefinition"]["UserGroup"],
        "g"
    );
    assert_eq!(
        team["WorkforceArn"],
        "arn:aws:sagemaker:us-east-1:000000000000:workforce/default"
    );
    let updated = resp_json(
        &run(
            &s,
            "UpdateWorkteam",
            json!({"WorkteamName": "team", "Description": "reviewers"}),
        )
        .unwrap(),
    );
    assert_eq!(updated["Workteam"]["Description"], "reviewers");
}

#[test]
fn describe_human_task_ui_reports_the_template_hash() {
    let s = svc();
    run(
        &s,
        "CreateHumanTaskUi",
        json!({"HumanTaskUiName": "ui", "UiTemplate": {"Content": "<p>hi</p>"}}),
    )
    .unwrap();
    let got = resp_json(&run(&s, "DescribeHumanTaskUi", json!({"HumanTaskUiName": "ui"})).unwrap());
    // sha256("<p>hi</p>")
    use sha2::Digest;
    let want: String = sha2::Sha256::digest(b"<p>hi</p>")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(got["UiTemplate"]["ContentSha256"], want);
    assert!(got["UiTemplate"].get("Content").is_none(), "{got}");
}

#[test]
fn describe_inference_component_reports_deployed_image_and_copy_counts() {
    let s = svc();
    run(
        &s,
        "CreateInferenceComponent",
        json!({
            "InferenceComponentName": "ic",
            "EndpointName": "ep",
            "VariantName": "v",
            "Specification": {"Container": {
                "Image": "123.dkr.ecr.us-east-1.amazonaws.com/img@sha256:abcd",
                "Environment": {"A": "1"}
            }},
            "RuntimeConfig": {"CopyCount": 2}
        }),
    )
    .unwrap();
    let got = resp_json(
        &run(
            &s,
            "DescribeInferenceComponent",
            json!({"InferenceComponentName": "ic"}),
        )
        .unwrap(),
    );
    let c = &got["Specification"]["Container"];
    assert_eq!(
        c["DeployedImage"]["SpecifiedImage"],
        "123.dkr.ecr.us-east-1.amazonaws.com/img@sha256:abcd"
    );
    assert_eq!(
        c["DeployedImage"]["ResolvedImage"],
        c["DeployedImage"]["SpecifiedImage"]
    );
    assert!(c["DeployedImage"]["ResolutionTime"].is_number(), "{c}");
    assert_eq!(c["Environment"]["A"], "1");
    assert!(c.get("Image").is_none(), "{c}");
    assert_eq!(got["RuntimeConfig"]["DesiredCopyCount"], 2);
    assert_eq!(got["RuntimeConfig"]["CurrentCopyCount"], 2);
    assert!(got["RuntimeConfig"].get("CopyCount").is_none(), "{got}");

    run(
        &s,
        "UpdateInferenceComponentRuntimeConfig",
        json!({"InferenceComponentName": "ic", "DesiredRuntimeConfig": {"CopyCount": 5}}),
    )
    .unwrap();
    let got = resp_json(
        &run(
            &s,
            "DescribeInferenceComponent",
            json!({"InferenceComponentName": "ic"}),
        )
        .unwrap(),
    );
    assert_eq!(got["RuntimeConfig"]["DesiredCopyCount"], 5);
    assert_eq!(got["RuntimeConfig"]["CurrentCopyCount"], 5);
}

#[test]
fn describe_cluster_reports_nested_configuration_as_details() {
    let s = svc();
    run(
        &s,
        "CreateCluster",
        json!({
            "ClusterName": "c1",
            "InstanceGroups": [{
                "InstanceGroupName": "workers",
                "InstanceType": "ml.c5.xlarge",
                "InstanceCount": 2,
                "MinInstanceCount": 1,
                "ImageId": "ami-0123456789abcdef0",
                "ExecutionRole": "arn:aws:iam::000000000000:role/r",
                "LifeCycleConfig": {"SourceS3Uri": "s3://b/p", "OnCreate": "on_create.sh"},
                "InstanceRequirements": {"InstanceTypes": ["ml.c5.xlarge"]},
                "KubernetesConfig": {"Labels": {"team": "ml"}}
            }],
            "RestrictedInstanceGroups": [{
                "InstanceGroupName": "rig",
                "InstanceType": "ml.p5.48xlarge",
                "InstanceCount": 3,
                "ExecutionRole": "arn:aws:iam::000000000000:role/r",
                "EnvironmentConfig": {}
            }],
            "AutoScaling": {"Mode": "Enable"}
        }),
    )
    .unwrap();
    let got = resp_json(&run(&s, "DescribeCluster", json!({"ClusterName": "c1"})).unwrap());
    let g = &got["InstanceGroups"][0];
    // A regular group is reported as `ClusterInstanceGroupDetails`.
    assert_eq!(g["InstanceGroupName"], "workers");
    assert_eq!(g["CurrentCount"], 2);
    assert_eq!(g["TargetCount"], 2);
    assert_eq!(g["Status"], "InService");
    assert_eq!(g["LifeCycleConfig"]["OnCreate"], "on_create.sh");
    assert!(g.get("InstanceCount").is_none(), "{g}");
    assert_eq!(g["MinCount"], 1);
    assert_eq!(g["CurrentImageId"], "ami-0123456789abcdef0");
    assert_eq!(g["DesiredImageId"], "ami-0123456789abcdef0");
    assert!(
        g.get("ImageId").is_none() && g.get("MinInstanceCount").is_none(),
        "{g}"
    );
    assert_eq!(
        g["InstanceRequirements"]["CurrentInstanceTypes"],
        json!(["ml.c5.xlarge"])
    );
    assert_eq!(
        g["InstanceRequirements"]["DesiredInstanceTypes"],
        json!(["ml.c5.xlarge"])
    );
    assert_eq!(g["KubernetesConfig"]["CurrentLabels"]["team"], "ml");
    assert_eq!(g["KubernetesConfig"]["DesiredLabels"]["team"], "ml");
    assert!(g["KubernetesConfig"].get("Labels").is_none(), "{g}");
    let r = &got["RestrictedInstanceGroups"][0];
    assert_eq!(r["CurrentCount"], 3);
    assert_eq!(r["TargetCount"], 3);
    assert_eq!(r["Status"], "InService");
    assert!(r.get("InstanceCount").is_none(), "{r}");
    assert_eq!(got["AutoScaling"]["Mode"], "Enable");
    assert_eq!(got["AutoScaling"]["Status"], "InService");

    // An update addressed by ARN changes the cluster, not its name.
    let arn = got["ClusterArn"].as_str().unwrap().to_string();
    run(
        &s,
        "UpdateCluster",
        json!({"ClusterName": arn, "NodeRecovery": "None", "InstanceGroups": []}),
    )
    .unwrap();
    let got = resp_json(&run(&s, "DescribeCluster", json!({"ClusterName": "c1"})).unwrap());
    assert_eq!(got["ClusterName"], "c1");
    assert_eq!(got["NodeRecovery"], "None");
}

#[test]
fn partial_update_structures_merge_into_the_stored_value() {
    let s = svc();
    run(
        &s,
        "CreateCodeRepository",
        json!({"CodeRepositoryName": "repo", "GitConfig": {
            "RepositoryUrl": "https://github.com/o/r.git", "Branch": "main"
        }}),
    )
    .unwrap();
    run(
        &s,
        "UpdateCodeRepository",
        json!({"CodeRepositoryName": "repo", "GitConfig": {
            "SecretArn": "arn:aws:secretsmanager:us-east-1:000000000000:secret:s"
        }}),
    )
    .unwrap();
    let got = resp_json(
        &run(
            &s,
            "DescribeCodeRepository",
            json!({"CodeRepositoryName": "repo"}),
        )
        .unwrap(),
    );
    assert_eq!(
        got["GitConfig"]["RepositoryUrl"],
        "https://github.com/o/r.git"
    );
    assert_eq!(got["GitConfig"]["Branch"], "main");
    assert_eq!(
        got["GitConfig"]["SecretArn"],
        "arn:aws:secretsmanager:us-east-1:000000000000:secret:s"
    );
}

#[test]
fn update_domain_settings_for_update_merge_into_domain_settings() {
    let s = svc();
    let created = resp_json(
        &run(
            &s,
            "CreateDomain",
            json!({
                "DomainName": "d1",
                "AuthMode": "IAM",
                "DefaultUserSettings": {"ExecutionRole": "arn:aws:iam::000000000000:role/r"},
                "SubnetIds": ["subnet-1"],
                "VpcId": "vpc-1",
                "DomainSettings": {
                    "SecurityGroupIds": ["sg-1"],
                    "RStudioServerProDomainSettings": {
                        "DomainExecutionRoleArn": "arn:aws:iam::000000000000:role/rs",
                        "RStudioConnectUrl": "https://connect"
                    }
                }
            }),
        )
        .unwrap(),
    );
    let arn = created["DomainArn"].as_str().unwrap();
    let id = arn.rsplit('/').next().unwrap().to_string();
    run(
        &s,
        "UpdateDomain",
        json!({
            "DomainId": id,
            "DomainSettingsForUpdate": {
                "SecurityGroupIds": ["sg-2"],
                "RStudioServerProDomainSettingsForUpdate": {
                    "DomainExecutionRoleArn": "arn:aws:iam::000000000000:role/rs2"
                }
            }
        }),
    )
    .unwrap();
    let got = resp_json(&run(&s, "DescribeDomain", json!({"DomainId": id})).unwrap());
    let settings = &got["DomainSettings"];
    assert_eq!(settings["SecurityGroupIds"], json!(["sg-2"]));
    let rs = &settings["RStudioServerProDomainSettings"];
    assert_eq!(
        rs["DomainExecutionRoleArn"],
        "arn:aws:iam::000000000000:role/rs2"
    );
    // Members the update did not carry keep their stored value.
    assert_eq!(rs["RStudioConnectUrl"], "https://connect");
    assert!(got.get("DomainSettingsForUpdate").is_none(), "{got}");
}

#[test]
fn update_training_job_disable_profiler_turns_profiling_off() {
    let s = svc();
    run(
        &s,
        "CreateTrainingJob",
        json!({
            "TrainingJobName": "tj",
            "RoleArn": "arn:aws:iam::000000000000:role/r",
            "AlgorithmSpecification": {"TrainingInputMode": "File", "TrainingImage": "img"},
            "OutputDataConfig": {"S3OutputPath": "s3://b/out"},
            "ResourceConfig": {"InstanceType": "ml.m5.large", "InstanceCount": 1, "VolumeSizeInGB": 10},
            "StoppingCondition": {"MaxRuntimeInSeconds": 3600},
            "ProfilerConfig": {"S3OutputPath": "s3://b/prof", "ProfilingIntervalInMilliseconds": 500}
        }),
    )
    .unwrap();
    let got = resp_json(&run(&s, "DescribeTrainingJob", json!({"TrainingJobName": "tj"})).unwrap());
    assert_eq!(got["ProfilingStatus"], "Enabled");
    run(
        &s,
        "UpdateTrainingJob",
        json!({
            "TrainingJobName": "tj",
            "ProfilerConfig": {"DisableProfiler": true},
            "ResourceConfig": {"KeepAlivePeriodInSeconds": 120}
        }),
    )
    .unwrap();
    let got = resp_json(&run(&s, "DescribeTrainingJob", json!({"TrainingJobName": "tj"})).unwrap());
    assert_eq!(got["ProfilingStatus"], "Disabled");
    assert_eq!(got["ProfilerConfig"]["DisableProfiler"], true);
    assert_eq!(got["ProfilerConfig"]["S3OutputPath"], "s3://b/prof");
    assert_eq!(got["ResourceConfig"]["KeepAlivePeriodInSeconds"], 120);
    assert_eq!(got["ResourceConfig"]["InstanceType"], "ml.m5.large");
}

#[test]
fn feature_group_switched_to_on_demand_reports_no_provisioned_units() {
    let s = svc();
    run(
        &s,
        "CreateFeatureGroup",
        json!({
            "FeatureGroupName": "fg",
            "RecordIdentifierFeatureName": "id",
            "EventTimeFeatureName": "t",
            "FeatureDefinitions": [
                {"FeatureName": "id", "FeatureType": "String"},
                {"FeatureName": "t", "FeatureType": "String"}
            ],
            "OnlineStoreConfig": {"EnableOnlineStore": true},
            "ThroughputConfig": {
                "ThroughputMode": "Provisioned",
                "ProvisionedReadCapacityUnits": 5,
                "ProvisionedWriteCapacityUnits": 5
            }
        }),
    )
    .unwrap();
    run(
        &s,
        "UpdateFeatureGroup",
        json!({"FeatureGroupName": "fg", "ThroughputConfig": {"ThroughputMode": "OnDemand"}}),
    )
    .unwrap();
    let got = resp_json(
        &run(
            &s,
            "DescribeFeatureGroup",
            json!({"FeatureGroupName": "fg"}),
        )
        .unwrap(),
    );
    assert_eq!(got["ThroughputConfig"]["ThroughputMode"], "OnDemand");
    assert!(
        got["ThroughputConfig"]
            .get("ProvisionedReadCapacityUnits")
            .is_none(),
        "{got}"
    );
    assert_eq!(got["OnlineStoreConfig"]["EnableOnlineStore"], true);
}
