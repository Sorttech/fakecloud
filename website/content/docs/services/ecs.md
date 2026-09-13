+++
title = "ECS"
description = "Elastic Container Service — full API: clusters, task definitions, real Fargate-style task execution via Docker, services with rolling deployments, task sets, container instances, ECS Exec."
weight = 22
+++

fakecloud implements Amazon Elastic Container Service (ECS) with full API coverage. 76 operations.

**Status: full API.** Covers clusters, task definitions, real Fargate-style task execution, services with rolling deployments + CODE_DEPLOY blue/green task sets, daemons, ExpressGatewayService, task sets, container instances, capacity providers, attributes, task protection, ECS Exec, placement constraints / strategies, awsvpc ENI binding, and the agent-side `Submit*` / `DiscoverPollEndpoint` surface.

## Supported today (full API)

- **Clusters** — `CreateCluster`, `DescribeClusters`, `DeleteCluster`, `ListClusters`, `UpdateCluster`, `UpdateClusterSettings`, `PutClusterCapacityProviders`
- **Task definitions** — `RegisterTaskDefinition`, `DescribeTaskDefinition`, `DeregisterTaskDefinition`, `DeleteTaskDefinitions`, `ListTaskDefinitions`, `ListTaskDefinitionFamilies`
- **Tasks** — `RunTask`, `StartTask`, `StopTask`, `DescribeTasks`, `ListTasks` with real Fargate-style execution via Docker/Podman
- **Services** — `CreateService`, `UpdateService`, `DeleteService`, `DescribeServices`, `ListServices`, `ListServicesByNamespace` with desired-count enforcement and rolling deployments
- **Service deployments** — `StopServiceDeployment`, `ListServiceDeployments`, `DescribeServiceDeployments`, `DescribeServiceRevisions`
- **Task sets** — `CreateTaskSet`, `UpdateTaskSet`, `DeleteTaskSet`, `DescribeTaskSets`, `UpdateServicePrimaryTaskSet` (EXTERNAL deployment controller)
- **Container instances** — `RegisterContainerInstance`, `DeregisterContainerInstance`, `DescribeContainerInstances`, `ListContainerInstances`, `UpdateContainerAgent`, `UpdateContainerInstancesState`
- **Attributes** — `PutAttributes`, `DeleteAttributes`, `ListAttributes`
- **Capacity providers** — `CreateCapacityProvider`, `DeleteCapacityProvider`, `DescribeCapacityProviders`, `UpdateCapacityProvider`
- **Task protection** — `GetTaskProtection`, `UpdateTaskProtection`
- **ECS Exec** — `ExecuteCommand` proxies to `docker exec` against the task's running container
- **Agent surface** — `SubmitContainerStateChange`, `SubmitTaskStateChange`, `SubmitAttachmentStateChanges`, `DiscoverPollEndpoint`
- **Tagging** — `TagResource`, `UntagResource`, `ListTagsForResource` (clusters and task definitions)
- **Account settings** — `PutAccountSetting`, `PutAccountSettingDefault`, `DeleteAccountSetting`, `ListAccountSettings`
- **Daemons** — `RegisterDaemonTaskDefinition`, `DescribeDaemonTaskDefinition`, `DeleteDaemonTaskDefinition`, `ListDaemonTaskDefinitions`, `CreateDaemon`, `DescribeDaemon`, `UpdateDaemon`, `DeleteDaemon`, `ListDaemons`, `DescribeDaemonDeployments`, `ListDaemonDeployments`, `DescribeDaemonRevisions`. `CreateDaemon` spawns one task per matching capacity-provider host so the daemon scales with the cluster.
- **Express gateway services** — `CreateExpressGatewayService`, `DescribeExpressGatewayService`, `UpdateExpressGatewayService`, `DeleteExpressGatewayService` (2026 ECS Express deployment controller)

### Services + rolling deployments

`CreateService` spawns tasks to match `desiredCount` under the service, tagging each with `startedBy=ecs-svc/<name>` so the tasks reconcile back to the service. `UpdateService` supports two independent mutations:

- **Scale** — set a new `desiredCount`. The service spawns additional tasks when scaling up and flips excess tasks to `desiredStatus=STOPPED` (runtime kill on the container) when scaling down.
- **Rolling deployment** — pass a new `taskDefinition`. The service marks the previous PRIMARY deployment as `ACTIVE`, creates a new `PRIMARY` deployment for the target revision, and drains tasks on the old task definition while new ones come up. Deployment circuit breaker + `minimumHealthyPercent` / `maximumPercent` are honoured in `deploymentConfiguration`.

`DeleteService` refuses while `desiredCount > 0` unless `force=true`; the forced path scales to 0 and stops every running task under the service before removing it.

#### Placement constraints + strategies

Both `RunTask` and `CreateService` honour `placementConstraints[]` (`distinctInstance`, `memberOf <expression>` against container-instance attributes) and `placementStrategy[]` (`random`, `spread` by attribute, `binpack` by `cpu` / `memory`). The scheduler ranks eligible container instances per strategy before launching; tasks fail with `unable to place a task` when no instance satisfies the constraints, matching real ECS.

#### awsvpc networking

Task definitions with `networkMode=awsvpc` allocate a per-task ENI from the subnet supplied via `networkConfiguration.awsvpcConfiguration.subnets[]`. The ENI is recorded on the task's `attachments[]` (`type=ElasticNetworkInterface`) with `privateIPv4Address`, `subnetId`, and `securityGroups[]` filled in. `assignPublicIp=ENABLED` flags the ENI as having a public address. Tasks sharing the same `awsvpc` network mode each get a distinct ENI; subnet exhaustion fails the task with `stopCode=TaskFailedToStart`.

#### CODE_DEPLOY blue/green task sets

Services declared with `deploymentController.type=CODE_DEPLOY` skip the in-line rolling deployment and require external task-set churn. `CreateTaskSet` registers a non-primary (`BLUE`) set, `UpdateServicePrimaryTaskSet` flips traffic, and the old set is drained on `DeleteTaskSet`. This matches the AWS deployment pattern CodeDeploy uses to drive blue/green cutovers.

Task-definition families track revisions monotonically; `DeleteTaskDefinitions` requires `DeregisterTaskDefinition` first (real AWS behaviour), and the result flips status to `DELETE_IN_PROGRESS`.

## Task execution

`RunTask` records the task synchronously and kicks off a background docker execution per spawned task:

1. `docker pull <image>` (timestamps captured on the task: `pullStartedAt` / `pullStoppedAt`). As with the ECS agent's default `ECS_IMAGE_PULL_BEHAVIOR`, a failed pull falls back to the image already cached locally, and a pull the registry rate limits (`429 Too Many Requests`, common for anonymous `public.ecr.aws` pulls from a shared IP) is retried with backoff. A pull that fails with nothing cached stops the task with `TaskFailedToStart`.
2. `docker run -d <image>` (container ID recorded on the task's container)
3. `docker wait <id>` (blocks on container exit; exit code → `containers[].exitCode`)
4. `docker logs <id>` (captured stdout/stderr stored on the task + exposed via the introspection endpoint)
5. `docker rm <id>` (cleanup)

Environment variables from the task definition are forwarded with `localhost` / `127.0.0.1` rewritten to `host.docker.internal` so containers reach fakecloud itself the same way Lambda does. `ECS_CONTAINER_METADATA_URI` and `ECS_CONTAINER_METADATA_URI_V4` are also injected, pointing at fakecloud's task-metadata endpoint (`/_fakecloud/ecs/v3/{task_id}` and `/_fakecloud/ecs/v4/{task_id}`) so SDKs and sidecars that read task metadata work out of the box.

### Container definition fidelity

The runtime translates the following task-definition fields straight into `docker run` flags so containers see the same shape they would on real ECS:

- `linuxParameters.capabilities.add[]` / `drop[]` -> `--cap-add` / `--cap-drop`
- `linuxParameters.initProcessEnabled=true` -> `--init`
- `linuxParameters.sharedMemorySize` -> `--shm-size <MiB>m`
- `linuxParameters.tmpfs[]` -> `--tmpfs <containerPath>:size=<size>,...`
- `ulimits[]` -> `--ulimit <name>=<soft>:<hard>`
- `user` -> `--user`
- `readonlyRootFilesystem=true` -> `--read-only`
- `pseudoTerminal=true` -> `--tty`
- `stopTimeout` -> `--stop-timeout` (also bounds the SIGTERM→SIGKILL grace inside force-stop)
- `volumeConfigurations[]` (per-task EBS attachments) -> volume per task with size, encryption, and FS type recorded on the task's `attachments[]` and bind-mounted at the declared `mountPoint`

### awslogs flush before STOPPED

Before a task transitions to `STOPPED`, the runtime drains any buffered `awslogs` events for that task synchronously, so `GetLogEvents` immediately after a `DescribeTasks` STOPPED response sees every line the container emitted. No probabilistic delay, no flake-prone polling in tests.

Without a container runtime (docker/podman missing), `RunTask` still returns tasks but they immediately transition to `STOPPED` with `stopCode=TaskFailedToStart`. This keeps the API surface shape-correct so tests on CI agents without Docker can still drive the control-plane surface.

### Pulling from fakecloud ECR

Tasks that reference AWS private-ECR URIs (`<account>.dkr.ecr.<region>.amazonaws.com/<repo>:<tag>`) are resolved against fakecloud's own OCI v2 endpoint. The runtime pulls from `127.0.0.1:<port>/<repo>:<tag>`, retags to the AWS URI, and runs the container under the user-visible image name. On Linux this is transparent because the docker daemon auto-treats `127.0.0.1` as an insecure registry. On Docker Desktop for macOS or Windows, the daemon runs in a VM and `127.0.0.1` maps to the VM itself, not the host — for full fidelity there, add `127.0.0.0/8` to Docker Desktop's insecure-registries and ensure fakecloud is reachable from the VM (e.g. via `host.docker.internal` forwarding).

Same resolution path applies to Lambda functions deployed with `PackageType=Image`: `Code.ImageUri` pointing at a fakecloud ECR URI is pulled and run on invoke.

### awslogs -> CloudWatch Logs

Container definitions that declare the `awslogs` log driver get their captured stdout/stderr forwarded to fakecloud's CloudWatch Logs service:

```json
"logConfiguration": {
  "logDriver": "awslogs",
  "options": {
    "awslogs-group": "/ecs/my-service",
    "awslogs-stream-prefix": "app",
    "awslogs-region": "us-east-1",
    "awslogs-create-group": "true"
  }
}
```

The runtime creates the log group on demand when `awslogs-create-group=true`, creates a stream named `<prefix>/<container-name>/<task-id>`, and appends one `LogEvent` per captured line. The usual fakecloud-logs API (`DescribeLogStreams`, `GetLogEvents`, subscription filters) then sees the container's output with no additional wiring.

### loadBalancers -> ELBv2 RegisterTargets

Services declared with `loadBalancers[]` register each task's primary IP (awsvpc) or container-instance/container-port pair (bridge/host) into the matching ELBv2 target group via `RegisterTargets` when the task transitions to `RUNNING`, and `DeregisterTargets` when it drains. The cutover happens inline as part of the rolling deployment so health-check and target-state assertions on the target group match what real ECS would produce.

### EventBridge task state change events

Task state transitions fire `aws.ecs` / `ECS Task State Change` events on the default EventBridge bus. Event `detail` carries the task ARN, cluster ARN, last status, stop code / reason on STOPPED, and a summary of each container including exit code. Rules matching `source: aws.ecs` receive these events and can route to SQS, SNS, Lambda, Step Functions — the standard target fan-out.

### Task role credentials

Tasks registered with a `taskRoleArn` get `AWS_CONTAINER_CREDENTIALS_FULL_URI` injected into every container. The URL points at a fakecloud-local endpoint — `http://host.docker.internal:<port>/_fakecloud/ecs/creds/<task-id>` — that returns IMDS-format credentials:

```json
{
  "AccessKeyId": "ASIA...",
  "SecretAccessKey": "...",
  "Token": "...",
  "Expiration": "2026-04-24T12:00:00Z",
  "RoleArn": "arn:aws:iam::123456789012:role/app-task-role"
}
```

AWS SDKs pick this up via the default credential-provider chain, so `aws sts get-caller-identity` (and any other SDK call) from inside the container works out of the box. fakecloud's STS accepts any `AccessKeyId`, so no pre-registration of the role is needed.

### Volumes + mount points

Task-definition `volumes[]` and per-container `mountPoints[]` translate into real `docker run -v` flags so containers see actual files at the paths they expect. Supported volume kinds:

- **Host bind** — `volume.host.sourcePath` is bind-mounted directly. A file written by the container shows up on the host path after the task stops.
- **EFS** — `efsVolumeConfiguration.fileSystemId` resolves to a host-side stub directory under `/tmp/fakecloud/efs/<filesystemId>[/<rootDirectory>]`. Multiple tasks targeting the same filesystem id share the stub, so a writer task and a reader task can exchange data the same way they would on real EFS.
- **FSx for Windows** — `fsxWindowsFileServerVolumeConfiguration.fileSystemId` resolves to an analogous stub under `/tmp/fakecloud/fsx/<filesystemId>/<rootDirectory>`.
- **Docker named volume** — `dockerVolumeConfiguration` passes the volume name through verbatim; docker creates the named volume on first reference.

`mountPoints[].readOnly` is honoured by appending `:ro` to the rendered `-v` flag.

### Secrets injection

Container definitions can pull secrets from SecretsManager or SSM Parameter Store via the standard `secrets[]` field:

```json
"secrets": [
  { "name": "DB_PASSWORD", "valueFrom": "arn:aws:secretsmanager:us-east-1:123456789012:secret:db-password-AbCdEf" },
  { "name": "API_KEY",     "valueFrom": "arn:aws:ssm:us-east-1:123456789012:parameter/app/api-key" }
]
```

The runtime resolves both kinds of ARN synchronously against the in-process state and injects the values as environment variables before `docker run`. A missing secret or parameter fails the task with `stopCode=TaskFailedToStart`, matching real ECS's "failed to retrieve secret" behaviour.

## Protocol

JSON protocol over `POST /`, with `X-Amz-Target: AmazonEC2ContainerServiceV20141113.<Action>`. Request + response bodies are JSON; tags use lowercase `key` / `value` (matches AWS SDK serialization).

## Introspection

Endpoints bypass the public AWS API so tests can assert deterministic state without pagination or role-assumption noise.

| Endpoint | Method | Purpose |
|---|---|---|
| `/_fakecloud/ecs/clusters` | GET | Dump every cluster across all accounts |
| `/_fakecloud/ecs/tasks` | GET | Dump every task; filter with `?cluster=` / `?status=` |
| `/_fakecloud/ecs/tasks/{taskId}` | GET | Single-task deep detail |
| `/_fakecloud/ecs/tasks/{taskId}/logs` | GET | Captured docker stdout/stderr + exit code |
| `/_fakecloud/ecs/tasks/{taskId}/force-stop` | POST | SIGTERM + SIGKILL the running container |
| `/_fakecloud/ecs/tasks/{taskId}/mark-failed` | POST | Flip to STOPPED without killing the container (inject exit code + reason) |
| `/_fakecloud/ecs/events` | GET | Replay the lifecycle event log |
| `/_fakecloud/ecs/metadata/{taskArn}` | GET | Aggregated v4 metadata dump keyed by full task ARN (URL-encode it) |

### Task metadata by ARN

`GET /_fakecloud/ecs/metadata/{taskArn}` returns the same shape a container would see at `ECS_CONTAINER_METADATA_URI_V4`, but addressable from outside the container by full task ARN -- handy when a test holds the `RunTask` response and wants to assert what the in-container SDK metadata loader would observe.

The path segment must be URL-encoded (`arn:aws:ecs:us-east-1:000000000000:task/cluster/<id>` -> `arn%3Aaws%3Aecs%3Aus-east-1%3A000000000000%3Atask%2Fcluster%2F<id>`). SDK helpers (`getEcsTaskMetadata` / `get_task_metadata`) handle the encoding for you.

```json
{
  "task": {
    "cluster": "prod",
    "taskArn": "arn:aws:ecs:us-east-1:000000000000:task/prod/abc123",
    "family": "web",
    "revision": 4,
    "desiredStatus": "RUNNING",
    "knownStatus": "RUNNING",
    "containers": [
      {
        "name": "app",
        "image": "nginx:1.27",
        "imageId": "sha256:...",
        "ports": [{"containerPort": 8080, "hostPort": 32768, "protocol": "tcp"}],
        "labels": {},
        "desiredStatus": "RUNNING",
        "knownStatus": "RUNNING",
        "limits": {"cpu": 0.25, "memory": 512},
        "createdAt": "2026-05-11T10:00:00Z",
        "startedAt": "2026-05-11T10:00:02Z"
      }
    ],
    "pullStartedAt": "2026-05-11T10:00:00Z",
    "pullStoppedAt": "2026-05-11T10:00:01Z",
    "availabilityZone": "us-east-1a",
    "launchType": "FARGATE",
    "vpcId": "vpc-fakecloud",
    "eniId": "eni-..."
  }
}
```

All endpoints are sorted deterministically (by ARN for clusters/tasks, by timestamp for events) so test assertions don't flake on map iteration order.

### Clusters dump

```json
{
  "clusters": [
    {
      "clusterName": "prod",
      "clusterArn": "arn:aws:ecs:us-east-1:111122223333:cluster/prod",
      "status": "ACTIVE",
      "runningTasksCount": 0,
      "pendingTasksCount": 0,
      "activeServicesCount": 0,
      "registeredContainerInstancesCount": 0,
      "capacityProviders": ["FARGATE"],
      "tags": [{"key": "env", "value": "prod"}],
      "createdAt": "2026-04-23T23:00:00+00:00"
    }
  ]
}
```

## SDK usage (testing helper)

The fakecloud client SDKs ship typed wrappers for every introspection endpoint. Use them instead of poking `/_fakecloud/*` paths by hand.

```go
// Go
clusters, _ := fakecloud.New("http://localhost:4566").ECS().GetClusters(ctx)
```

```python
# Python
async with FakeCloud() as fc:
    clusters = await fc.ecs.get_clusters()
```

```typescript
// TypeScript
const fc = new FakeCloud();
const { clusters } = await fc.ecs.getClusters();
```

```rust
// Rust
let fc = FakeCloud::new("http://localhost:4566");
let clusters = fc.ecs().get_clusters().await?;
```

## Source

- [`crates/fakecloud-ecs`](https://github.com/faiscadev/fakecloud/tree/main/crates/fakecloud-ecs)
- [AWS ECS API reference](https://docs.aws.amazon.com/AmazonECS/latest/APIReference/Welcome.html)
