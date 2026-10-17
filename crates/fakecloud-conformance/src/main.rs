#![allow(dead_code)]

use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command as ProcessCommand, Stdio};

/// Guard that kills the child process on drop.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Guard for a container-backed server: asks it to shut down gracefully so it
/// runs every runtime's `stop_all()` and removes the containers it started,
/// and only kills it if it has not exited within the grace period.
struct GracefulChild(Child);

impl Drop for GracefulChild {
    fn drop(&mut self) {
        let terminated = ProcessCommand::new("kill")
            .arg("-TERM")
            .arg(self.0.id().to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if terminated {
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while std::time::Instant::now() < deadline {
                if let Ok(Some(_)) = self.0.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
use std::time::Duration;

mod checksum;
mod generators;
mod probe;
mod report;
mod shape_validator;
mod smithy;

#[derive(Parser)]
#[command(name = "fakecloud-conformance", about = "AWS API conformance testing")]
struct Cli {
    /// Path to the aws-models directory
    #[arg(long, default_value = "aws-models")]
    models_dir: PathBuf,

    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Print all operations per service from the Smithy models
    Operations,
    /// Print model checksums for all operations
    Checksums,
    /// Run Level 1 auto-generated conformance probes
    Run {
        /// Only test these services (comma-separated)
        #[arg(long)]
        services: Option<String>,
        /// Output format: text or json
        #[arg(long, default_value = "text")]
        format: String,
        /// Connect to an already-running fakecloud at this endpoint
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// Run Level 2 audit: check handwritten test coverage
    Audit,
    /// Check that each crate's vendored `model.json` still matches `aws-models/`
    VendoredModels,
    /// Check conformance results against baseline (fails if coverage drops)
    Check {
        /// Path to conformance-baseline.json
        #[arg(long, default_value = "conformance-baseline.json")]
        baseline: PathBuf,
        /// Connect to an already-running fakecloud at this endpoint
        #[arg(long)]
        endpoint: Option<String>,
        /// Also write the full JSON report to this path
        #[arg(long)]
        json_out: Option<PathBuf>,
        /// Also write a compact markdown summary (suitable for $GITHUB_STEP_SUMMARY) to this path
        #[arg(long)]
        markdown_summary_out: Option<PathBuf>,
    },
    /// Update the baseline file with current conformance results
    UpdateBaseline {
        /// Path to conformance-baseline.json
        #[arg(long, default_value = "conformance-baseline.json")]
        baseline: PathBuf,
        /// Connect to an already-running fakecloud at this endpoint
        #[arg(long)]
        endpoint: Option<String>,
    },
}

/// A handful of service crates `include_str!` their own copy of the Smithy
/// model for model-driven input validation, because `cargo publish` only
/// packages files under the crate root and cannot reach `aws-models/`. Those
/// copies go stale silently every time the weekly refresh lands, so check them.
fn cmd_vendored_models(project_root: &std::path::Path, models_dir: &std::path::Path) -> bool {
    let crates_dir = project_root.join("crates");
    let Ok(entries) = std::fs::read_dir(&crates_dir) else {
        eprintln!("cannot read {}", crates_dir.display());
        return false;
    };
    let mut vendored: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let copy = entry.path().join("model.json");
        if !copy.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let service = name.strip_prefix("fakecloud-").unwrap_or(&name).to_string();
        vendored.push((service, copy));
    }
    vendored.sort();

    println!("=== Vendored Smithy models ===");
    let mut stale = Vec::new();
    for (service, copy) in &vendored {
        let source = if models_dir.is_absolute() {
            models_dir.join(format!("{service}.json"))
        } else {
            project_root
                .join(models_dir)
                .join(format!("{service}.json"))
        };
        let a = match std::fs::read(copy) {
            Ok(a) => a,
            Err(e) => {
                println!("  [x] {service}: cannot read {} ({e})", copy.display());
                stale.push(service.clone());
                continue;
            }
        };
        // A crate whose directory suffix is not its model's file name (the
        // `elbv2` / `elasticloadbalancingv2` shape) lands here: name the
        // mismatch rather than reporting a stale copy.
        let b = match std::fs::read(&source) {
            Ok(b) => b,
            Err(e) => {
                println!(
                    "  [x] {service}: no model at {} ({e}) -- does the crate suffix match the aws-models file name?",
                    source.display()
                );
                stale.push(service.clone());
                continue;
            }
        };
        if a == b {
            println!("  [ok] {service}");
        } else {
            println!(
                "  [x] {service}: {} differs from {}",
                copy.display(),
                source.display()
            );
            stale.push(service.clone());
        }
    }

    if stale.is_empty() {
        println!(
            "\nPASS: all {} vendored models match aws-models",
            vendored.len()
        );
        return true;
    }
    println!(
        "\nFAIL: {} vendored model(s) stale: {}",
        stale.len(),
        stale.join(", ")
    );
    println!("Fix with: cp aws-models/<service>.json crates/fakecloud-<service>/model.json");
    false
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        CliCommand::Operations => cmd_operations(&cli.models_dir),
        CliCommand::Checksums => cmd_checksums(&cli.models_dir),
        CliCommand::Run {
            services,
            format,
            endpoint,
        } => cmd_run(&cli.models_dir, services, &format, endpoint),
        CliCommand::Audit => {
            let project_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..");
            let pass = fakecloud_conformance::audit::run_audit(&project_root);
            if !pass {
                std::process::exit(1);
            }
        }
        CliCommand::VendoredModels => {
            let project_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..");
            if !cmd_vendored_models(&project_root, &cli.models_dir) {
                std::process::exit(1);
            }
        }
        CliCommand::Check {
            baseline,
            endpoint,
            json_out,
            markdown_summary_out,
        } => cmd_check(
            &cli.models_dir,
            &baseline,
            endpoint,
            json_out.as_deref(),
            markdown_summary_out.as_deref(),
        ),
        CliCommand::UpdateBaseline { baseline, endpoint } => {
            cmd_update_baseline(&cli.models_dir, &baseline, endpoint)
        }
    }
}

fn cmd_operations(models_dir: &std::path::Path) {
    let models = load_models(models_dir);

    let mut total_ops = 0;
    for (service_name, model) in &models {
        let count = model.operations.len();
        total_ops += count;
        println!("{} ({} operations)", service_name, count);
        for op in &model.operations {
            let input_members = op
                .input_shape
                .as_ref()
                .and_then(|id| model.shapes.get(id))
                .map(|s| match &s.shape_type {
                    smithy::ShapeType::Structure { members } => members
                        .iter()
                        .filter(|m| m.required)
                        .map(|m| m.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    _ => String::new(),
                })
                .unwrap_or_default();

            if input_members.is_empty() {
                println!("  {}", op.name);
            } else {
                println!("  {} (required: {})", op.name, input_members);
            }
        }
        println!();
    }
    println!(
        "Total: {} operations across {} services",
        total_ops,
        models.len()
    );
}

fn cmd_checksums(models_dir: &std::path::Path) {
    let models = checksum::model_checksums(models_dir).unwrap_or_else(|e| {
        eprintln!("Error computing checksums: {}", e);
        std::process::exit(1);
    });

    for model in &models {
        println!("{}:", model.service_name);
        for (op, cs) in &model.operations {
            println!("  {}  {}", cs, op);
        }
        println!();
    }
}

/// Run probes and return the report data.
fn run_probes(
    models_dir: &std::path::Path,
    services_filter: Option<String>,
    endpoint: Option<String>,
) -> report::ConformanceReport {
    let models = load_models(models_dir);

    let filter: Option<Vec<String>> =
        services_filter.map(|s| s.split(',').map(|s| s.trim().to_string()).collect());

    let (endpoint, _server) = if let Some(ep) = endpoint {
        (ep, None)
    } else {
        let (ep, child) = start_fakecloud();
        (ep, Some(ChildGuard(child)))
    };
    // Variants re-probed against a container-backed server (see
    // `needs_container_runtime`). Only when we own the sweep server: against
    // an operator-supplied endpoint we cannot choose its runtime.
    let may_reprobe = _server.is_some();
    let mut reprobe_cli: Option<Option<String>> = None;

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("Failed to create HTTP client");

    // Cross-reference Smithy operations against fakecloud's own
    // `fn supported_actions()` lists. Operations not in a service's SUPPORTED
    // set are classified as NotImplemented without sending a probe. This
    // prevents fakecloud's grab-bag of unrouted-path responses
    // (NotFoundException, UnknownOperationException, execute-api stage errors,
    // generic-handler stubs, etc.) from being counted as "routed successes"
    // under the lenient 2xx-4xx = Pass rule.
    //
    // The SUPPORTED list is fakecloud's own source of truth for "we implement
    // this action." Treating it as authoritative gives honest per-service
    // coverage numbers and surfaces real feature gaps — the same numbers the
    // Level-2 audit uses. If a service routes actions without listing them
    // in SUPPORTED (a known under-reporting pattern in some older services),
    // the right fix is to update SUPPORTED, not to paper over in the probe.
    let project_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let implemented_actions = fakecloud_conformance::audit::scan_implemented_actions(&project_root)
        .unwrap_or_else(|e| {
            eprintln!("Warning: failed to scan implemented actions: {e}");
            HashMap::new()
        });
    let implemented_tags = fakecloud_conformance::audit::audit_service_tags(&project_root);

    let mut all_results: HashMap<String, HashMap<String, Vec<probe::ProbeResult>>> = HashMap::new();
    let mut total_ops_per_service: HashMap<String, usize> = HashMap::new();

    for (service_name, model) in &models {
        if let Some(ref filter) = filter {
            if !filter.contains(service_name) {
                continue;
            }
        }

        total_ops_per_service.insert(service_name.clone(), model.operations.len());

        eprintln!(
            "Probing {} ({} operations)...",
            service_name,
            model.operations.len()
        );

        let mut service_results: HashMap<String, Vec<probe::ProbeResult>> = HashMap::new();

        // Union SUPPORTED lists across every audit mapping whose tag list
        // includes this Smithy service_name. Covers shared-crate services
        // (bedrock + bedrock-runtime share one SUPPORTED list across two
        // Smithy service keys).
        let supported_for_service: Option<Vec<String>> = {
            let mut combined: Vec<String> = Vec::new();
            let mut any = false;
            for (audit_key, tags) in &implemented_tags {
                if tags.iter().any(|t| t == service_name) {
                    if let Some(list) = implemented_actions.get(audit_key) {
                        combined.extend(list.iter().cloned());
                        any = true;
                    }
                }
            }
            if any {
                combined.sort();
                combined.dedup();
                Some(combined)
            } else {
                // Smithy service not yet in audit mapping — preserve the
                // legacy probe-and-classify behavior so nothing regresses.
                None
            }
        };

        for op in &model.operations {
            // Re-seed before every op so destructive ops (DeleteBucket /
            // DeleteObject) don't leave later ops staring at a missing
            // resource. Cheap: each call is a single keep-alive request
            // against the in-process server.
            seed_service_resources(&client, &endpoint, service_name);

            let overrides = HashMap::new();
            let variants = generators::generate_all_variants(model, &op.name, &overrides);

            // If fakecloud's SUPPORTED list is known and this op isn't in it,
            // short-circuit every variant to NotImplemented. Saves the
            // network round-trip and, more importantly, gives an honest
            // per-service coverage number.
            if let Some(supported) = supported_for_service.as_ref() {
                if !supported.iter().any(|s| s == &op.name) {
                    let results: Vec<probe::ProbeResult> = variants
                        .iter()
                        .map(|v| probe::ProbeResult {
                            variant_name: v.name.clone(),
                            status: probe::ProbeStatus::NotImplemented,
                            http_status: 0,
                            response_body: String::new(),
                            duration_ms: 0,
                        })
                        .collect();
                    let total = results.len();
                    eprintln!("  SKIP {} (0/{})", op.name, total);
                    service_results.insert(op.name.clone(), results);
                    continue;
                }
            }

            // Per-op seed for state one operation's documented `@examples`
            // assume. The guard undoes it when this iteration ends -- normally,
            // or by unwinding from a panicking variant thread -- so no other op
            // sees it.
            let _op_seed = seed_operation_state(&client, &endpoint, service_name, &op.name);

            // Get output shape for shape validation
            let output_shape_id = op.output_shape.as_deref();

            // Probe variants with bounded concurrency to avoid overwhelming fakecloud
            let max_concurrent = 8;
            let mut op_results: Vec<probe::ProbeResult> = variants
                .chunks(max_concurrent)
                .flat_map(|chunk| {
                    std::thread::scope(|s| {
                        let handles: Vec<_> = chunk
                            .iter()
                            .map(|variant| {
                                let client = &client;
                                let endpoint = &endpoint;
                                let service_name = service_name.as_str();
                                let op_name = op.name.as_str();
                                let model_info = output_shape_id.map(|oid| (model, oid));
                                s.spawn(move || {
                                    probe::probe_variant_with_model(
                                        client,
                                        endpoint,
                                        service_name,
                                        op_name,
                                        variant,
                                        model_info,
                                    )
                                })
                            })
                            .collect();
                        handles
                            .into_iter()
                            .map(|h| h.join().unwrap())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();

            if may_reprobe {
                for (variant, result) in variants.iter().zip(op_results.iter_mut()) {
                    if !needs_container_runtime(result) {
                        continue;
                    }
                    let cli = reprobe_cli.get_or_insert_with(|| {
                        let cli = reprobe_container_cli(
                            std::env::var_os("FAKECLOUD_CONTAINER_CLI"),
                            fakecloud_core::container_net::detect_container_cli,
                        );
                        if cli.is_none() {
                            eprintln!(
                                "  note: some variants need a container runtime and none is \
                                 available; they are reported as failures"
                            );
                        }
                        cli
                    });
                    let Some(cli) = cli.as_deref() else {
                        continue;
                    };
                    eprintln!(
                        "  RETRY {} {} against a {}-backed server",
                        op.name, variant.name, cli
                    );
                    *result = probe_with_container_runtime(
                        cli,
                        &client,
                        service_name,
                        &op.name,
                        variant,
                        output_shape_id.map(|oid| (model, oid)),
                    );
                }
            }

            let passed = op_results
                .iter()
                .filter(|r| r.status == probe::ProbeStatus::Pass)
                .count();
            let total = op_results.len();
            let marker = if op_results
                .iter()
                .all(|r| r.status == probe::ProbeStatus::NotImplemented)
            {
                "SKIP"
            } else if passed == total {
                "OK"
            } else {
                "FAIL"
            };
            eprintln!("  {} {} ({}/{})", marker, op.name, passed, total);

            service_results.insert(op.name.clone(), op_results);
        }

        all_results.insert(service_name.clone(), service_results);
    }

    report::build_report(all_results, &total_ops_per_service)
}

fn cmd_run(
    models_dir: &std::path::Path,
    services_filter: Option<String>,
    format: &str,
    endpoint: Option<String>,
) {
    let report_data = run_probes(models_dir, services_filter, endpoint);
    match format {
        "json" => report::print_json_report(&report_data),
        _ => report::print_text_report(&report_data),
    }
}

/// Seed any resources the probe driver assumes exist before sending success
/// expectations. Today only S3 needs this: the REST request builder
/// hard-codes `test-conformance-bucket` and `test-key` into every operation
/// URL (see `probe::rest_request_config`), so ops like HeadBucket /
/// GetObjectAcl / ListObjects can't return 2xx unless the bucket and a
/// stub object actually exist on the server before the run starts.
///
/// Other services either generate per-op resources inside the variant input
/// (Lambda's `test-conformance-function`, ECR's `test-conformance-repository`,
/// etc. are typically created by their own Create* probe before later ops
/// reach them) or don't depend on pre-existing state at all (Query/Json
/// services where every probe is self-contained). Calling this for those
/// service names is a no-op.
fn seed_service_resources(client: &reqwest::blocking::Client, endpoint: &str, service_name: &str) {
    match service_name {
        "s3" => seed_s3(client, endpoint),
        "secretsmanager" => seed_secretsmanager(client, endpoint),
        _ => {}
    }
}

fn seed_s3(client: &reqwest::blocking::Client, endpoint: &str) {
    let auth = "AWS4-HMAC-SHA256 Credential=test/20240101/us-east-1/s3/aws4_request, \
                SignedHeaders=host;x-amz-date, Signature=00";
    // Best-effort: ignore conflicts (bucket/object may already exist if the
    // server preserved state across runs) and network errors (the harness
    // will surface those as crashes during probing anyway).
    let bucket_url = format!("{}/test-conformance-bucket", endpoint);
    let _ = client.put(&bucket_url).header("Authorization", auth).send();
    let object_url = format!("{}/test-conformance-bucket/test-key", endpoint);
    let _ = client
        .put(&object_url)
        .header("Authorization", auth)
        .header("Content-Type", "application/octet-stream")
        .body("conformance-seed")
        .send();
}

/// Pre-seed the secret used by the documented `@examples` for Secrets Manager
/// (`MyTestDatabaseSecret`) with both a description and a resource-based
/// policy. Without this seed, the `GetResourcePolicy` example diff fails
/// because the live response omits `ResourcePolicy` for secrets that have
/// none attached. Best-effort: ignores conflicts when the secret already
/// exists from a previous probe run.
fn seed_secretsmanager(client: &reqwest::blocking::Client, endpoint: &str) {
    let auth = "AWS4-HMAC-SHA256 Credential=test/20240101/us-east-1/secretsmanager/aws4_request, \
                SignedHeaders=host;x-amz-date, Signature=00";
    let send = |target: &str, body: &str| {
        let _ = client
            .post(endpoint)
            .header("Content-Type", "application/x-amz-json-1.1")
            .header("X-Amz-Target", format!("secretsmanager.{}", target))
            .header("Authorization", auth)
            .body(body.to_string())
            .send();
    };
    send(
        "CreateSecret",
        r#"{"Name":"MyTestDatabaseSecret","Description":"My test database secret","SecretString":"conformance-seed"}"#,
    );
    send(
        "PutResourcePolicy",
        r#"{"SecretId":"MyTestDatabaseSecret","ResourcePolicy":"{\"Version\":\"2012-10-17\",\"Statement\":[{\"Effect\":\"Allow\",\"Principal\":{\"AWS\":\"arn:aws:iam::123456789012:root\"},\"Action\":\"secretsmanager:GetSecretValue\",\"Resource\":\"*\"}]}"}"#,
    );
}

const LAMBDA_SEED_AUTH: &str =
    "AWS4-HMAC-SHA256 Credential=test/20240101/us-east-1/lambda/aws4_request, \
                                SignedHeaders=host;x-amz-date, Signature=00";

/// State seeded for a single operation. Dropping it puts back what was there
/// before, so the seed cannot outlive its operation even if probing panics.
struct OperationSeedGuard<'a> {
    client: &'a reqwest::blocking::Client,
    endpoint: &'a str,
    seed: OperationSeed,
}

enum OperationSeed {
    /// `my-function`'s reserved concurrency before the seed (`None`: unset).
    LambdaReservedConcurrency(Option<i64>),
}

impl Drop for OperationSeedGuard<'_> {
    fn drop(&mut self) {
        match self.seed {
            OperationSeed::LambdaReservedConcurrency(previous) => {
                let url = lambda_example_concurrency_url(self.endpoint);
                let _ = match previous {
                    Some(n) => self
                        .client
                        .put(&url)
                        .header("Content-Type", "application/json")
                        .header("Authorization", LAMBDA_SEED_AUTH)
                        .body(format!(r#"{{"ReservedConcurrentExecutions":{n}}}"#))
                        .send(),
                    None => self
                        .client
                        .delete(&url)
                        .header("Authorization", LAMBDA_SEED_AUTH)
                        .send(),
                };
            }
        }
    }
}

fn lambda_example_concurrency_url(endpoint: &str) -> String {
    format!("{endpoint}/2017-10-31/functions/my-function/concurrency")
}

/// Seed state that exactly one operation's documented `@examples` assume,
/// right before that operation's variants run.
///
/// Lambda `GetFunctionConcurrency`: its example reads `my-function` (created
/// by the `CreateFunction` example) expecting `ReservedConcurrentExecutions:
/// 250`. Without a reservation AWS (and fakecloud) answer with an empty body,
/// and `DeleteFunctionConcurrency`'s example, which runs earlier, clears any
/// reservation. The previous setting is recorded so dropping the returned
/// guard leaves every other operation's view of the function unchanged. Best-effort:
/// when `my-function` doesn't exist the seed is a 404 and the restore a no-op.
fn seed_operation_state<'a>(
    client: &'a reqwest::blocking::Client,
    endpoint: &'a str,
    service_name: &str,
    op_name: &str,
) -> Option<OperationSeedGuard<'a>> {
    if (service_name, op_name) != ("lambda", "GetFunctionConcurrency") {
        return None;
    }
    let url = lambda_example_concurrency_url(endpoint);
    let current = client
        .get(&url)
        .header("Authorization", LAMBDA_SEED_AUTH)
        .send()
        .ok()
        .filter(|r| r.status().is_success())?;
    let previous = current
        .json::<serde_json::Value>()
        .ok()
        .and_then(|v| v["ReservedConcurrentExecutions"].as_i64());
    client
        .put(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", LAMBDA_SEED_AUTH)
        .body(r#"{"ReservedConcurrentExecutions":250}"#)
        .send()
        .ok()
        .filter(|r| r.status().is_success())?;
    Some(OperationSeedGuard {
        client,
        endpoint,
        seed: OperationSeed::LambdaReservedConcurrency(previous),
    })
}

fn load_models(models_dir: &std::path::Path) -> Vec<(String, smithy::ServiceModel)> {
    smithy::load_all_models(models_dir).unwrap_or_else(|e| {
        eprintln!("Error loading models: {}", e);
        std::process::exit(1);
    })
}

/// The container-CLI value the probe-sweep server should run with, given the
/// operator's current `FAKECLOUD_CONTAINER_CLI` (as `var_os`). Returns `Some`
/// sentinel binary name to force every container runtime onto its metadata-only
/// path when the operator hasn't pinned a CLI, and `None` (no override) when
/// they have. Kept pure so the precedence is unit-testable without spawning a
/// server. See `start_fakecloud` for why the sweep avoids real containers.
fn disabling_container_cli(current: Option<std::ffi::OsString>) -> Option<&'static str> {
    match current {
        Some(_) => None,
        None => Some("fakecloud-no-container-backend"),
    }
}

/// True when a variant failed only because the sweep server runs without a
/// container runtime: the operation refused with the runtime-required error
/// every container-backed service raises (RDS CreateDBInstance, for one, will
/// not report an instance it cannot back with a real database). Such a
/// variant says nothing about API conformance until it runs where a runtime
/// exists.
///
/// Only a 5xx refusal counts: the hint can also appear as data in a successful
/// response (an ECS task's `stoppedReason`), and a shape failure on such a
/// response is a real finding that a fresh server must not paper over.
fn needs_container_runtime(result: &probe::ProbeResult) -> bool {
    result.status != probe::ProbeStatus::Pass
        && result.http_status >= 500
        && result
            .response_body
            .contains(fakecloud_core::container_net::CONTAINER_RUNTIME_HINT)
}

/// The container CLI to re-probe runtime-dependent variants with. An operator
/// who pinned `FAKECLOUD_CONTAINER_CLI` already chose the sweep server's
/// runtime (a real CLI means nothing failed for lack of one; a bogus one means
/// they opted out), so only an unset variable triggers auto-detection.
fn reprobe_container_cli(
    current: Option<std::ffi::OsString>,
    detect: impl FnOnce() -> Option<String>,
) -> Option<String> {
    match current {
        Some(_) => None,
        None => detect(),
    }
}

/// Probe one variant against a fresh server that drives real containers, then
/// shut that server down gracefully so it removes what it started. A fresh
/// server per variant keeps variants from colliding on resource names the
/// way a shared one would (two documented examples creating the same DB
/// instance identifier).
fn probe_with_container_runtime(
    cli: &str,
    client: &reqwest::blocking::Client,
    service_name: &str,
    op_name: &str,
    variant: &generators::TestVariant,
    model_info: Option<(&smithy::ServiceModel, &str)>,
) -> probe::ProbeResult {
    let (endpoint, child) = match try_start_fakecloud(Some(cli), Duration::from_secs(60)) {
        Ok(started) => started,
        Err(e) => {
            eprintln!("  could not start a {cli}-backed server: {e}");
            return probe::ProbeResult {
                variant_name: variant.name.clone(),
                status: probe::ProbeStatus::UnexpectedResult(format!(
                    "needs a container runtime, but a {cli}-backed server failed to start: {e}"
                )),
                http_status: 0,
                response_body: String::new(),
                duration_ms: 0,
            };
        }
    };
    let pid = child.id();
    let server = GracefulChild(child);
    seed_service_resources(client, &endpoint, service_name);
    let result = probe::probe_variant_with_model(
        client,
        &endpoint,
        service_name,
        op_name,
        variant,
        model_info,
    );
    // The operation may have started a container in the background (RDS
    // CreateDBInstance returns `creating` and runs the engine afterwards). A
    // container is only tracked for shutdown once its `run` returns, so let
    // the server's container-CLI children finish before asking it to stop.
    wait_for_child_processes(pid, Duration::from_secs(300));
    drop(server);
    remove_labelled_leftovers(cli, pid);
    result
}

/// Wait until `pid` has had no child processes for a short settle period, or
/// `limit` elapses. fakecloud drives the container runtime through its CLI, so
/// a live child means a container operation is still in flight.
fn wait_for_child_processes(pid: u32, limit: Duration) {
    let deadline = std::time::Instant::now() + limit;
    let mut quiet = 0;
    while std::time::Instant::now() < deadline && quiet < 4 {
        let busy = ProcessCommand::new("pgrep")
            .arg("-P")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        quiet = if busy { 0 } else { quiet + 1 };
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Remove any container or volume still carrying the stopped server's
/// ownership label. Graceful shutdown already removes what the server tracked;
/// this catches anything created too late for it to see.
fn remove_labelled_leftovers(cli: &str, pid: u32) {
    let label = format!("label=fakecloud-instance=fakecloud-{pid}");
    for (list, remove) in [
        (&["ps", "-aq", "--filter"][..], &["rm", "-f"][..]),
        (
            &["volume", "ls", "-q", "--filter"][..],
            &["volume", "rm", "-f"][..],
        ),
    ] {
        let Ok(out) = ProcessCommand::new(cli).args(list).arg(&label).output() else {
            continue;
        };
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !ids.is_empty() {
            let _ = ProcessCommand::new(cli)
                .args(remove)
                .args(&ids)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// Start the main sweep server (containers disabled, see `try_start_fakecloud`),
/// exiting the process if it cannot come up: nothing can be probed without it.
fn start_fakecloud() -> (String, Child) {
    try_start_fakecloud(None, Duration::from_secs(60)).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    })
}

/// Start a fakecloud server for the sweep. `container_cli` is `None` for the
/// main sweep server (containers disabled, see below) and `Some(cli)` for the
/// short-lived servers that re-probe runtime-dependent variants, which get a
/// longer `ready_within`: with a runtime enabled the server reaps stale
/// containers before it binds, which can take several bounded CLI calls.
fn try_start_fakecloud(
    container_cli: Option<&str>,
    ready_within: Duration,
) -> Result<(String, Child), String> {
    let port = find_available_port();
    let endpoint = format!("http://127.0.0.1:{}", port);

    let bin = find_binary();

    let mut cmd = ProcessCommand::new(&bin);
    cmd.arg("--addr")
        .arg(format!("127.0.0.1:{}", port))
        .arg("--log-level")
        .arg("error")
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // The probe sweep exercises every operation of every service for API-shape
    // conformance; it never asserts on runtime behaviour (that is e2e's job).
    // But container-backed services (EC2 RunInstances, Lambda Invoke, ECS
    // RunTask, RDS/ElastiCache create) would each shell out to Docker and spawn
    // real containers + per-subnet networks for thousands of probe variants.
    // On a CI runner that pulls base images and piles up containers until the
    // host starves and the runner loses contact with GitHub (~2h, the "lost
    // communication with the server" death introduced by EC2 VPC isolation in
    // #1745). Most synchronous API responses are identical with or without a
    // backing container, so we point the container-CLI probe at a binary that
    // does not exist: every runtime's detect_container_cli() returns None and
    // they serve their documented metadata-only path. The few operations that
    // refuse outright without a runtime are re-probed one variant at a time
    // against a container-backed server (`needs_container_runtime`), which
    // bounds the containers a sweep can start. An operator who genuinely
    // wants the sweep to drive real containers can still set
    // FAKECLOUD_CONTAINER_CLI explicitly to override this default.
    if let Some(cli) = container_cli {
        cmd.env("FAKECLOUD_CONTAINER_CLI", cli);
    } else if let Some(sentinel) =
        disabling_container_cli(std::env::var_os("FAKECLOUD_CONTAINER_CLI"))
    {
        cmd.env("FAKECLOUD_CONTAINER_CLI", sentinel);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to start fakecloud ({bin}): {e}"))?;

    let deadline = std::time::Instant::now() + ready_within;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok() {
            eprintln!("fakecloud started on {}", endpoint);
            return Ok((endpoint, child));
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("fakecloud exited during startup ({status})"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let _ = child.kill();
    let _ = child.wait();
    Err(format!(
        "fakecloud did not start within {} seconds",
        ready_within.as_secs()
    ))
}

fn find_available_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind to random port")
        .local_addr()
        .unwrap()
        .port()
}

fn find_binary() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let debug_path = format!("{}/../../target/debug/fakecloud", manifest_dir);
    let release_path = format!("{}/../../target/release/fakecloud", manifest_dir);

    if std::path::Path::new(&debug_path).exists() {
        return debug_path;
    }
    if std::path::Path::new(&release_path).exists() {
        return release_path;
    }

    eprintln!(
        "fakecloud binary not found. Run `cargo build` first.\nLooked in:\n  {}\n  {}",
        debug_path, release_path
    );
    std::process::exit(1);
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Baseline {
    variants_passed: usize,
    total_variants: usize,
    per_service: HashMap<String, ServiceBaseline>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct ServiceBaseline {
    passed: usize,
    total: usize,
}

fn cmd_check(
    models_dir: &std::path::Path,
    baseline_path: &std::path::Path,
    endpoint: Option<String>,
    json_out: Option<&std::path::Path>,
    markdown_summary_out: Option<&std::path::Path>,
) {
    let baseline_content = std::fs::read_to_string(baseline_path).unwrap_or_else(|e| {
        eprintln!("Failed to read baseline {}: {}", baseline_path.display(), e);
        std::process::exit(1);
    });
    let baseline: Baseline = serde_json::from_str(&baseline_content).unwrap_or_else(|e| {
        eprintln!("Failed to parse baseline: {}", e);
        std::process::exit(1);
    });

    let report_data = run_probes(models_dir, None, endpoint);

    report::print_text_report(&report_data);

    if let Some(path) = json_out {
        let json = serde_json::to_string_pretty(&report_data).unwrap();
        if let Err(e) = std::fs::write(path, format!("{}\n", json)) {
            eprintln!("Failed to write JSON report {}: {}", path.display(), e);
        }
    }

    // Every implemented service is at 100%, so any failing variant is a
    // regression: report each failing operation.
    let mut regressions = failing_operations(&report_data);

    // Per-service ratchet against the baseline, so a service can't keep a
    // clean 100% by losing variants. No margin by default; a local run can
    // relax it via FAKECLOUD_CONFORMANCE_FLAKE_MARGIN.
    let flake_margin: usize = std::env::var("FAKECLOUD_CONFORMANCE_FLAKE_MARGIN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let current_by_service: HashMap<&str, usize> = report_data
        .services
        .iter()
        .map(|svc| {
            let passed: usize = svc.operations.iter().map(|o| o.passed).sum();
            (svc.service_name.as_str(), passed)
        })
        .collect();

    for (svc_name, svc_baseline) in &baseline.per_service {
        let current_passed = current_by_service
            .get(svc_name.as_str())
            .copied()
            .unwrap_or(0);
        let allowed = svc_baseline.passed.saturating_sub(flake_margin);
        if current_passed < allowed {
            regressions.push(format!(
                "{}: {} → {} variants passing (was {}, lost {}; margin {})",
                svc_name,
                svc_baseline.passed,
                current_passed,
                svc_baseline.passed,
                svc_baseline.passed - current_passed,
                flake_margin,
            ));
        }
    }

    // Overall ratchet: per-service margins can pile up, so allow the union.
    let overall_margin = flake_margin.saturating_mul(baseline.per_service.len().max(1));
    let current_total_passed = report_data.summary.variants_passed;
    let allowed_total = baseline.variants_passed.saturating_sub(overall_margin);
    if current_total_passed < allowed_total {
        regressions.push(format!(
            "overall: {} → {} variants passing (lost {}; margin {})",
            baseline.variants_passed,
            current_total_passed,
            baseline.variants_passed - current_total_passed,
            overall_margin,
        ));
    }

    if let Some(path) = markdown_summary_out {
        let baseline_passed: HashMap<String, usize> = baseline
            .per_service
            .iter()
            .map(|(k, v)| (k.clone(), v.passed))
            .collect();
        let md = report::render_markdown_summary(
            &report_data,
            baseline.variants_passed,
            baseline.total_variants,
            &baseline_passed,
            &regressions,
        );
        if let Err(e) = std::fs::write(path, md) {
            eprintln!("Failed to write markdown summary {}: {}", path.display(), e);
        }
    }

    if regressions.is_empty() {
        println!("\nConformance check PASSED (every variant passes, no regressions)");
        println!(
            "  baseline: {}/{} ({:.1}%)",
            baseline.variants_passed,
            baseline.total_variants,
            baseline.variants_passed as f64 / baseline.total_variants as f64 * 100.0,
        );
        println!(
            "  current:  {}/{} ({:.1}%)",
            report_data.summary.variants_passed,
            report_data.summary.total_variants,
            report_data.summary.variants_passed as f64 / report_data.summary.total_variants as f64
                * 100.0,
        );
    } else {
        eprintln!("\nConformance check FAILED:");
        for r in &regressions {
            eprintln!("  {}", r);
        }
        eprintln!("\nTo update the baseline after intentional changes:");
        eprintln!("  cargo run -p fakecloud-conformance -- update-baseline");
        std::process::exit(1);
    }
}

/// One line per operation with a failing variant: `service Op: passed/total`.
fn failing_operations(report_data: &report::ConformanceReport) -> Vec<String> {
    report_data
        .services
        .iter()
        .flat_map(|svc| {
            svc.operations
                .iter()
                .filter(|op| op.passed < op.total_variants)
                .map(|op| {
                    format!(
                        "{} {}: {}/{} variants passing",
                        svc.service_name, op.name, op.passed, op.total_variants
                    )
                })
        })
        .collect()
}

fn cmd_update_baseline(
    models_dir: &std::path::Path,
    baseline_path: &std::path::Path,
    endpoint: Option<String>,
) {
    let report_data = run_probes(models_dir, None, endpoint);

    let mut per_service = HashMap::new();
    for svc in &report_data.services {
        let passed: usize = svc.operations.iter().map(|o| o.passed).sum();
        let total: usize = svc.operations.iter().map(|o| o.total_variants).sum();
        per_service.insert(svc.service_name.clone(), ServiceBaseline { passed, total });
    }

    let baseline = Baseline {
        variants_passed: report_data.summary.variants_passed,
        total_variants: report_data.summary.total_variants,
        per_service,
    };

    let json = serde_json::to_string_pretty(&baseline).unwrap();
    std::fs::write(baseline_path, format!("{}\n", json)).unwrap_or_else(|e| {
        eprintln!("Failed to write baseline: {}", e);
        std::process::exit(1);
    });

    println!("Baseline updated: {}", baseline_path.display());
    println!(
        "  {}/{} variants passing ({:.1}%)",
        baseline.variants_passed,
        baseline.total_variants,
        baseline.variants_passed as f64 / baseline.total_variants as f64 * 100.0,
    );
}

#[cfg(test)]
mod tests {

    fn op(name: &str, passed: usize, total: usize) -> report::OperationReport {
        report::OperationReport {
            name: name.to_string(),
            total_variants: total,
            passed,
            failed: total - passed,
            not_implemented: false,
            crashes: 0,
            failures: Vec::new(),
        }
    }

    #[test]
    fn every_failing_operation_is_a_check_failure() {
        let report_data = report::ConformanceReport {
            services: vec![
                report::ServiceReport {
                    service_name: "sqs".to_string(),
                    total_operations: 2,
                    operations: vec![op("SendMessage", 30, 30), op("DeleteMessageBatch", 21, 22)],
                },
                report::ServiceReport {
                    service_name: "kms".to_string(),
                    total_operations: 1,
                    operations: vec![op("Encrypt", 40, 40)],
                },
            ],
            summary: report::Summary {
                total_operations: 3,
                implemented: 3,
                passing: 2,
                failing: 1,
                not_implemented: 0,
                total_variants: 92,
                variants_passed: 91,
                variants_failed: 1,
            },
        };
        assert_eq!(
            failing_operations(&report_data),
            vec!["sqs DeleteMessageBatch: 21/22 variants passing".to_string()]
        );
    }
    use super::*;

    /// A one-request-per-connection HTTP stub that answers `GET` with
    /// `get_body` and everything else with an empty 200, recording each
    /// request as `"METHOD path body"`.
    fn concurrency_stub(
        get_body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h == "\r\n" || h.is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let mut parts = line.split_whitespace();
                let (method, path) = (parts.next().unwrap(), parts.next().unwrap());
                log.lock().unwrap().push(format!(
                    "{method} {path} {}",
                    String::from_utf8_lossy(&body)
                ));
                let reply = if method == "GET" { get_body } else { "{}" };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
        });
        (endpoint, seen)
    }

    #[test]
    fn concurrency_seed_only_applies_to_get_function_concurrency() {
        let (endpoint, seen) = concurrency_stub("{}");
        let client = reqwest::blocking::Client::new();
        assert!(seed_operation_state(&client, &endpoint, "lambda", "GetFunction").is_none());
        assert!(seed_operation_state(&client, &endpoint, "s3", "GetFunctionConcurrency").is_none());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn concurrency_seed_is_removed_when_none_was_set() {
        let (endpoint, seen) = concurrency_stub("{}");
        let client = reqwest::blocking::Client::new();
        let guard = seed_operation_state(&client, &endpoint, "lambda", "GetFunctionConcurrency")
            .expect("seeded");
        drop(guard);
        let seen = seen.lock().unwrap().clone();
        let path = "/2017-10-31/functions/my-function/concurrency";
        assert_eq!(
            seen,
            vec![
                format!("GET {path} "),
                format!(r#"PUT {path} {{"ReservedConcurrentExecutions":250}}"#),
                format!("DELETE {path} "),
            ]
        );
    }

    #[test]
    fn concurrency_seed_restores_the_previous_value_even_on_panic() {
        let (endpoint, seen) = concurrency_stub(r#"{"ReservedConcurrentExecutions":7}"#);
        let client = reqwest::blocking::Client::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard =
                seed_operation_state(&client, &endpoint, "lambda", "GetFunctionConcurrency")
                    .expect("seeded");
            panic!("a variant thread panicked");
        }));
        assert!(result.is_err());
        let last = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            last,
            r#"PUT /2017-10-31/functions/my-function/concurrency {"ReservedConcurrentExecutions":7}"#
        );
    }

    #[test]
    fn sweep_disables_containers_when_cli_unset() {
        // Unset operator CLI -> inject the sentinel that no container CLI
        // resolves to, so the probe-sweep server serves metadata-only and
        // never spawns containers that would starve a CI runner.
        let sentinel = disabling_container_cli(None).expect("sentinel injected");
        assert_eq!(sentinel, "fakecloud-no-container-backend");
        // The sentinel must not resolve to a runnable binary, or a runtime's
        // `detect_container_cli()` would treat it as a live CLI and the sweep
        // would still spawn containers. This mirrors core's `cli_available`
        // liveness probe (`<cli> info` succeeds) without taking a dep on it.
        let resolves = ProcessCommand::new(sentinel)
            .arg("info")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!resolves, "sentinel must not be a runnable container CLI");
    }

    #[test]
    fn sweep_respects_explicit_container_cli() {
        // An operator who pins FAKECLOUD_CONTAINER_CLI keeps it: no override.
        assert_eq!(
            disabling_container_cli(Some(std::ffi::OsString::from("docker"))),
            None
        );
        assert_eq!(
            disabling_container_cli(Some(std::ffi::OsString::from(""))),
            None
        );
    }

    fn result(status: probe::ProbeStatus, body: &str) -> probe::ProbeResult {
        probe::ProbeResult {
            variant_name: "v".to_string(),
            status,
            http_status: 503,
            response_body: body.to_string(),
            duration_ms: 0,
        }
    }

    #[test]
    fn runtime_required_failures_are_reprobed() {
        let body = format!(
            "<Error><Code>InsufficientDBInstanceCapacity</Code><Message>Docker/Podman is \
             required for RDS DB instances but is not available. {}</Message></Error>",
            fakecloud_core::container_net::CONTAINER_RUNTIME_HINT
        );
        assert!(needs_container_runtime(&result(
            probe::ProbeStatus::UnexpectedResult("Expected success, got HTTP 503".into()),
            &body,
        )));
        // A pass (the documented error variants expect exactly this refusal)
        // and an unrelated failure are left alone.
        assert!(!needs_container_runtime(&result(
            probe::ProbeStatus::Pass,
            &body
        )));
        // The hint as data in a 2xx body (an ECS task's stoppedReason) is a
        // real shape finding, not a missing runtime.
        let mut ok_body = result(probe::ProbeStatus::ShapeMismatch("x".into()), &body);
        ok_body.http_status = 200;
        assert!(!needs_container_runtime(&ok_body));
        assert!(!needs_container_runtime(&result(
            probe::ProbeStatus::UnexpectedResult("Expected success, got HTTP 400".into()),
            "<Error><Code>InvalidParameterValue</Code></Error>",
        )));
    }

    #[test]
    fn reprobe_detects_a_runtime_only_when_the_operator_pinned_none() {
        assert_eq!(
            reprobe_container_cli(None, || Some("docker".to_string())),
            Some("docker".to_string())
        );
        assert_eq!(reprobe_container_cli(None, || None), None);
        // A pinned CLI already decided the sweep server's runtime, so nothing
        // is re-probed (and detection never runs).
        assert_eq!(
            reprobe_container_cli(Some(std::ffi::OsString::from("false")), || {
                panic!("must not detect when the operator pinned a CLI")
            }),
            None
        );
    }
}
