+++
title = "AWS Organizations"
description = "Organizations control plane + SCP enforcement as the top-of-chain IAM evaluator layer."
weight = 6
+++

fakecloud ships a minimal AWS Organizations implementation. Its purpose is to let you attach Service Control Policies (SCPs) to accounts and organizational units so your tests can exercise the full IAM evaluation hierarchy — SCP ceiling, permission boundary, session policy, identity policy, resource policy — end to end.

Both the control plane and SCP enforcement are live. SCPs act as the top-of-chain allow-list ceiling: see the [security reference](/docs/reference/security#phase-6-service-control-policies-scps) for the full evaluation order and the management-account and service-linked-role exemptions.

## Model

- Many independent organizations per fakecloud process. `CreateOrganization` sets the caller's account as the management account and seeds a root OU, and rejects only when **that account** is already in an organization (`AlreadyInOrganizationException`) -- another account having created one does not block it.
- An account belongs to at most one organization. Inviting an account another organization already holds is `HandshakeConstraintViolationException` with `Reason: ALREADY_IN_AN_ORGANIZATION`, both at invite time and again when the handshake is accepted.
- Organizations never see each other: every read resolves the caller's own organization, and a caller in none gets `AWSOrganizationsNotInUseException` -- the same answer as a process with no organizations at all. The exceptions are the operations that are cross-organization by nature, and they stay scoped to the caller's own involvement: `ListHandshakesForAccount` and `DescribeHandshake` (the account answering an invitation is not yet a member of the inviting organization, so a handshake resolves by id, readable by its two parties and by members of the organization that owns it), and the responsibility-transfer ops (a transfer is recorded once, in the source organization, and both management accounts are parties to it).
- `FullAWSAccess` is auto-created and auto-attached to the root OU on `CreateOrganization`, matching AWS. Its content:
  ```json
  {"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"*","Resource":"*"}]}
  ```
- Both feature sets are supported. `CONSOLIDATED_BILLING` creates the org with no policy types enabled, so SCPs are unavailable — matching AWS. Any other `FeatureSet` value is `InvalidInputException`.
- Only the management account can run write ops (`CreateOrganizationalUnit`, `MoveAccount`, `DeleteOrganization`, etc.). A member but non-management caller gets `AccessDeniedException`. A non-member caller gets `AWSOrganizationsNotInUseException` so org existence itself does not leak.
- Bootstrapping an admin via `/_fakecloud/iam/create-admin` leaves the account **standalone** — it joins no organization, matching AWS, where a freshly vended account belongs to no organization until it is invited and accepts, or is created through `CreateAccount`. Pass `"organizationId": "o-..."` in the request body to enroll the account into that organization's root OU instead, the shortcut equivalent of an `InviteAccountToOrganization` + `AcceptHandshake` pair.

## Supported operations

| Operation | Status | Notes |
|-----------|--------|-------|
| `CreateOrganization` | ✅ | `FeatureSet` `ALL` or `CONSOLIDATED_BILLING`; caller becomes management |
| `DescribeOrganization` | ✅ | Returns `AWSOrganizationsNotInUseException` to non-members |
| `DeleteOrganization` | ✅ | Management only; fails if any non-management members remain |
| `ListRoots` | ✅ | Returns the single root |
| `CreateOrganizationalUnit` | ✅ | Management only; name must be unique under the parent |
| `UpdateOrganizationalUnit` | ✅ | Rename; duplicate-name check |
| `DeleteOrganizationalUnit` | ✅ | Fails with `OrganizationalUnitNotEmptyException` if children remain |
| `DescribeOrganizationalUnit` | ✅ | |
| `ListOrganizationalUnitsForParent` | ✅ | |
| `ListAccounts` | ✅ | Returns all members |
| `ListAccountsForParent` | ✅ | |
| `DescribeAccount` | ✅ | |
| `MoveAccount` | ✅ | Enforces exact source-parent match |
| `CreatePolicy` | ✅ | `Type=SERVICE_CONTROL_POLICY` only; structural JSON validation |
| `UpdatePolicy` | ✅ | Blocks mutation of AWS-managed policies (`FullAWSAccess`) |
| `DeletePolicy` | ✅ | Blocks deletion when attached or AWS-managed |
| `DescribePolicy` | ✅ | |
| `ListPolicies` | ✅ | `Filter` required and must be `SERVICE_CONTROL_POLICY` |
| `AttachPolicy` | ✅ | Idempotent re-attach; targets = root / OU / account |
| `DetachPolicy` | ✅ | Returns `PolicyNotAttachedException` on missing attachment |
| `ListPoliciesForTarget` | ✅ | |
| `ListTargetsForPolicy` | ✅ | |

## SCP semantics

Policies are JSON documents of the same shape as identity policies. The control plane validates structural parseability; the evaluator applies the same document at request time, so there is no divergence between what the control plane accepts and what actually gates traffic.

`FullAWSAccess` (`p-FullAWSAccess`) is created and attached to the root OU on `CreateOrganization` and is immutable: `UpdatePolicy` and `DeletePolicy` return `PolicyChangesNotAllowedException` for it. You can still `DetachPolicy` it from the root — AWS permits this, and tests that want to exercise a restrictive SCP posture rely on being able to drop the AWS-managed allow-all.

SCPs are enforced through the IAM evaluator when `FAKECLOUD_IAM` is `soft` or `strict`. Off by default when no organization exists or the resolver returns a non-member / management / service-linked-role principal. See the [security reference](/docs/reference/security#phase-6-service-control-policies-scps) for the full evaluation chain.

## Usage

```rust
use aws_sdk_organizations::Client;

let client = Client::new(&sdk_config);
let created = client.create_organization().send().await?;
let org = created.organization().unwrap();
println!("{} managed by {}", org.id().unwrap(), org.master_account_id().unwrap());
```

## Behavior under `FAKECLOUD_IAM=off`

Organizations control plane works identically regardless of the `FAKECLOUD_IAM` setting. SCP enforcement is gated on IAM being enabled, so creating an organization with `FAKECLOUD_IAM=off` is allowed but has no effect on evaluation — the resolver is still wired, but the evaluator it feeds is never called.
