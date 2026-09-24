//! `OrganizationsService` billing-responsibility-transfer family:
//! `InviteOrganizationToTransferResponsibility`,
//! `DescribeResponsibilityTransfer`, `UpdateResponsibilityTransfer`,
//! `TerminateResponsibilityTransfer`, and the inbound/outbound list ops.

use super::*;
use crate::state::{random_id, ResponsibilityTransfer};
use chrono::DateTime;

/// The only transfer type AWS Organizations currently defines.
const TRANSFER_TYPE_BILLING: &str = "BILLING";

fn transfer_not_found(id: &str) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "ResponsibilityTransferNotFoundException",
        format!("No responsibility transfer was found with id {id}."),
    )
}

fn invalid_input(msg: &str) -> AwsServiceError {
    AwsServiceError::aws_error(StatusCode::BAD_REQUEST, "InvalidInputException", msg)
}

/// Validate the `Type` field against the `ResponsibilityTransferType`
/// enum. Only `BILLING` is defined today.
fn require_transfer_type(body: &Value) -> Result<String, AwsServiceError> {
    let t = required_str(body, "Type")?;
    if t != TRANSFER_TYPE_BILLING {
        return Err(invalid_input(&format!(
            "Type must be one of [BILLING], got {t}"
        )));
    }
    Ok(t.to_string())
}

/// Is `account_id` the target of `t`?
///
/// Delegates to the one shared predicate so this agrees with the
/// handshake gate: an account that can accept an invitation must also
/// be able to read and act on the transfer it accepted.
fn is_transfer_target(
    registry: &crate::state::OrganizationsRegistry,
    t: &ResponsibilityTransfer,
    account_id: &str,
) -> bool {
    t.target_management_account_id == account_id
        || registry.account_matches_target("EMAIL", &t.target_management_account_id, account_id)
}

fn is_transfer_party(
    registry: &crate::state::OrganizationsRegistry,
    t: &ResponsibilityTransfer,
    account_id: &str,
) -> bool {
    t.source_management_account_id == account_id || is_transfer_target(registry, t, account_id)
}

/// `TransferParticipant.ManagementAccountId` is modeled as an
/// `AccountId` (exactly 12 digits), so an email-targeted transfer -- whose
/// target is recorded as the address the source named -- carries only
/// the email until the address resolves to an account id.
fn target_participant(t: &ResponsibilityTransfer) -> Value {
    let mut party = json!({
        "ManagementAccountEmail": t.target_management_account_email,
    });
    if let Some(id) = crate::state::target_account_id("ACCOUNT", &t.target_management_account_id)
        .filter(|id| id.len() == 12 && id.chars().all(|c| c.is_ascii_digit()))
    {
        party["ManagementAccountId"] = json!(id);
    }
    party
}

fn transfer_payload(t: &ResponsibilityTransfer) -> Value {
    let mut obj = json!({
        "Arn": t.arn,
        "Name": t.name,
        "Id": t.id,
        "Type": t.transfer_type,
        "Status": t.status,
        "Source": {
            "ManagementAccountId": t.source_management_account_id,
            "ManagementAccountEmail": t.source_management_account_email,
        },
        "Target": target_participant(t),
        "StartTimestamp": t.start_timestamp.timestamp() as f64,
    });
    if let Some(end) = t.end_timestamp {
        obj["EndTimestamp"] = json!(end.timestamp() as f64);
    }
    if let Some(h) = &t.active_handshake_id {
        obj["ActiveHandshakeId"] = json!(h);
    }
    obj
}

impl OrganizationsService {
    /// Resolve a transfer for a MUTATING call and return the id of the
    /// organization that stores it.
    ///
    /// A transfer is an arrangement between two management accounts, and
    /// both can read it. `source_only` says whether this particular
    /// mutation is the source's alone: renaming is, because the source
    /// chose the name. Ending is the source's too while the offer is
    /// still open -- `WITHDRAWN` means the inviter pulled it, and the
    /// target's answer to an open offer is `DeclineHandshake`. Once the
    /// transfer is ACCEPTED the riding handshake is gone, so the target
    /// may end it as well, or it would have no way out of an
    /// arrangement it is actively carrying.
    ///
    /// Resolving through the caller's own organization instead reported
    /// "not found" to the target, which is indistinguishable from a bad
    /// id.
    fn party_org_of_transfer(
        &self,
        guard: &parking_lot::RwLockWriteGuard<'_, crate::state::OrganizationsRegistry>,
        id: &str,
        caller: &str,
        source_only: bool,
    ) -> Result<String, AwsServiceError> {
        let org = guard
            .org_of_responsibility_transfer(id)
            .ok_or_else(|| transfer_not_found(id))?;
        let transfer = org
            .responsibility_transfers
            .get(id)
            .ok_or_else(|| transfer_not_found(id))?;
        // A stranger learns nothing beyond "no such transfer".
        if !is_transfer_party(guard, transfer, caller) {
            return Err(transfer_not_found(id));
        }
        // Only an offer still awaiting an answer is the source's alone to
        // withdraw. A terminal status falls through, so the caller gets the
        // real "already in that status" answer rather than advice to
        // decline a handshake that no longer exists.
        if transfer.source_management_account_id != caller
            && (source_only || transfer.status == "REQUESTED")
        {
            return Err(AwsServiceError::aws_error(
                StatusCode::FORBIDDEN,
                "AccessDeniedException",
                if source_only {
                    "Only the source management account can rename a responsibility transfer."
                } else {
                    "Only the source management account can withdraw a transfer that has not \
                     been accepted; decline the handshake instead."
                },
            ));
        }
        Ok(org.org_id.clone())
    }
}

impl OrganizationsService {
    pub(super) fn invite_organization_to_transfer_responsibility(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let transfer_type = require_transfer_type(&body)?;
        let source_name = required_str(&body, "SourceName")?.to_string();
        // StartTimestamp is required; accept either an epoch number or an
        // ISO-8601 string and fall back to "now" if the SDK omitted it.
        let start = body
            .get("StartTimestamp")
            .and_then(json_to_datetime)
            .unwrap_or_else(Utc::now);
        let target_obj = body
            .get("Target")
            .ok_or_else(|| invalid_input("Target is required"))?;
        let target_id = target_obj
            .get("Id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_input("Target.Id is required"))?
            .to_string();
        let target_kind = target_obj
            .get("Type")
            .and_then(|v| v.as_str())
            .unwrap_or("ACCOUNT");
        // Same shape validation the account-invite path applies: without
        // it a mismatched target (an address under Type=ACCOUNT) is stored
        // as the target account id, and the handshake sits OPEN forever
        // because no caller can authenticate as that string.
        super::accounts::validate_invite_target(target_kind, &target_id)?;
        let notes = body
            .get("Notes")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let mut guard = self.state.write();
        // Authorize FIRST. Resolving the target before the management gate
        // turned "is this address registered?" into an oracle any caller
        // could read off the difference between InvalidInputException and
        // AWSOrganizationsNotInUseException.
        let source_org_id = self
            .management_org_mut(&mut guard, &req.account_id)?
            .org_id
            .clone();
        let registry = &*guard;

        // Record the target EXACTLY as the caller named it, as AWS does:
        // resolving an address against other organizations' management
        // accounts would answer "does this address exist, and what is its
        // account id?" for organizations the caller has nothing to do
        // with. The party gate resolves the other way instead -- it asks
        // the CALLER what its own address is (see `is_transfer_target`).
        let (target_account_id, target_email) = if target_kind == "EMAIL" {
            (target_id.clone(), target_id.clone())
        } else {
            (target_id.clone(), format!("{target_id}@example.com"))
        };
        // Naming your own organization is the one case there is nothing to
        // leak about. Comparing only against the management account let a
        // plain member of the SAME organization be named, which opened --
        // and let that member accept -- a "cross-organization" transfer
        // whose two ends were one organization.
        let target_is_own_org = registry.org_by_id(&source_org_id).is_some_and(|org| {
            org.accounts
                .keys()
                .any(|id| registry.account_matches_target(target_kind, &target_id, id))
        });
        if target_is_own_org {
            return Err(AwsServiceError::aws_error_with_fields(
                StatusCode::BAD_REQUEST,
                "HandshakeConstraintViolationException",
                "An organization cannot transfer responsibility to itself.",
                vec![(
                    "Reason".to_string(),
                    "SOURCE_AND_TARGET_CANNOT_MATCH".to_string(),
                )],
            ));
        }
        let org = guard
            .org_by_id_mut(&source_org_id)
            .expect("management gate resolved this organization");

        let now = Utc::now();
        // The transfer rides on a handshake the invited org accepts.
        let handshake_id = format!("h-{}", random_id(32));
        let handshake_arn = format!(
            "arn:aws:organizations::{}:handshake/{}/transfer/{}",
            org.management_account_id, org.org_id, handshake_id
        );
        let handshake = crate::state::Handshake {
            id: handshake_id.clone(),
            arn: handshake_arn,
            action: "TRANSFER_RESPONSIBILITY".to_string(),
            state: "OPEN".to_string(),
            requested_timestamp: now,
            expiration_timestamp: now + chrono::Duration::days(15),
            source_account_id: org.management_account_id.clone(),
            target_account_id: target_account_id.clone(),
            target_email: Some(target_email.clone()),
            target_kind: target_kind.to_string(),
            notes,
            organization_id: org.org_id.clone(),
        };
        org.handshakes
            .insert(handshake_id.clone(), handshake.clone());

        let transfer_id = format!("rt-{}", random_id(32));
        let transfer_arn = format!(
            "arn:aws:organizations::{}:responsibilitytransfer/{}/{}",
            org.management_account_id, org.org_id, transfer_id
        );
        let transfer = ResponsibilityTransfer {
            id: transfer_id.clone(),
            arn: transfer_arn,
            name: source_name,
            transfer_type,
            status: "REQUESTED".to_string(),
            direction: "OUTBOUND".to_string(),
            source_management_account_id: org.management_account_id.clone(),
            source_management_account_email: org.management_account_email.clone(),
            target_management_account_id: target_account_id,
            target_management_account_email: target_email,
            start_timestamp: start,
            end_timestamp: None,
            active_handshake_id: Some(handshake_id),
        };
        org.responsibility_transfers
            .insert(transfer_id, transfer.clone());

        Ok(AwsResponse::ok_json(
            json!({ "Handshake": handshake_payload(&handshake) }),
        ))
    }

    pub(super) fn describe_responsibility_transfer(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "Id")?.to_string();
        let guard = self.state.read();
        // A transfer is stored once, in the SOURCE organization, but it has
        // two parties: resolving it through the caller's own organization
        // would hide every inbound transfer from the account being invited
        // to take over billing.
        let transfer = guard
            .org_of_responsibility_transfer(&id)
            .and_then(|org| org.responsibility_transfers.get(&id))
            .filter(|t| is_transfer_party(&guard, t, &req.account_id))
            .ok_or_else(|| transfer_not_found(&id))?;
        Ok(AwsResponse::ok_json(
            json!({ "ResponsibilityTransfer": transfer_payload(transfer) }),
        ))
    }

    pub(super) fn update_responsibility_transfer(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "Id")?.to_string();
        let name = required_str(&body, "Name")?.to_string();
        let mut guard = self.state.write();
        let org_id = self.party_org_of_transfer(&guard, &id, &req.account_id, true)?;
        let transfer = guard
            .org_by_id_mut(&org_id)
            .and_then(|org| org.responsibility_transfers.get_mut(&id))
            .expect("resolved just above");
        transfer.name = name;
        let snapshot = transfer.clone();
        Ok(AwsResponse::ok_json(
            json!({ "ResponsibilityTransfer": transfer_payload(&snapshot) }),
        ))
    }

    pub(super) fn terminate_responsibility_transfer(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "Id")?.to_string();
        let end = body
            .get("EndTimestamp")
            .and_then(json_to_datetime)
            .unwrap_or_else(Utc::now);
        let mut guard = self.state.write();
        // Either party can end the arrangement.
        let org_id = self.party_org_of_transfer(&guard, &id, &req.account_id, false)?;
        let org = guard.org_by_id_mut(&org_id).expect("resolved just above");
        let transfer = org
            .responsibility_transfers
            .get_mut(&id)
            .expect("resolved just above");
        if transfer.status == "WITHDRAWN" {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "ResponsibilityTransferAlreadyInStatusException",
                "The responsibility transfer is already withdrawn.",
            ));
        }
        // AWS's op "ends a transfer", so it applies to one still awaiting an
        // answer AND to one already accepted and running. Only a transfer
        // that has already reached a terminal state cannot be ended.
        if !matches!(transfer.status.as_str(), "REQUESTED" | "ACCEPTED") {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "InvalidResponsibilityTransferTransitionException",
                format!(
                    "A responsibility transfer in status {} cannot be terminated.",
                    transfer.status
                ),
            ));
        }
        transfer.status = "WITHDRAWN".to_string();
        transfer.end_timestamp = Some(end);
        // Take the riding handshake id BEFORE clearing the field: reading it
        // back off the post-clear snapshot always saw `None`, so the
        // handshake stayed OPEN and the target could still accept a
        // withdrawn transfer.
        let riding_handshake = transfer.active_handshake_id.take();
        let snapshot = transfer.clone();
        // Cancel the riding handshake too.
        if let Some(hid) = &riding_handshake {
            if let Some(h) = org.handshakes.get_mut(hid) {
                h.state = "CANCELED".to_string();
            }
        }
        Ok(AwsResponse::ok_json(
            json!({ "ResponsibilityTransfer": transfer_payload(&snapshot) }),
        ))
    }

    pub(super) fn list_inbound_responsibility_transfers(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        self.list_responsibility_transfers(req, "INBOUND")
    }

    pub(super) fn list_outbound_responsibility_transfers(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        self.list_responsibility_transfers(req, "OUTBOUND")
    }

    /// AWS's `ResponsibilityTransfer` shape has no `Direction` member --
    /// direction is expressed by which operation you call, so the stored
    /// `direction` field is fakecloud-internal provenance surfaced only
    /// through introspection. Which list a transfer belongs to is decided
    /// by whether the caller is its source or its target.
    fn list_responsibility_transfers(
        &self,
        req: &AwsRequest,
        direction: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        // `Type` is required on both list ops.
        let transfer_type = require_transfer_type(&body)?;
        // Only the INBOUND request models an optional `Id` to fetch a
        // single transfer.
        let only_id = (direction == "INBOUND")
            .then(|| body.get("Id").and_then(|v| v.as_str()).map(str::to_string))
            .flatten();
        let (max_results, next_token) = parse_list_pagination(&body)?;
        let guard = self.state.read();
        // The caller must be in an organization at all -- these ops declare
        // AWSOrganizationsNotInUseException -- but the transfers it can see
        // are the ones it is a party to, which for INBOUND live in the other
        // organization.
        self.require_member(&guard, &req.account_id)?;
        let mut rows: Vec<&ResponsibilityTransfer> = guard
            .iter()
            .flat_map(|org| org.responsibility_transfers.values())
            .filter(|t| {
                only_id.as_deref().is_none_or(|id| t.id == id)
                    && t.transfer_type == transfer_type
                    && match direction {
                        "OUTBOUND" => t.source_management_account_id == req.account_id,
                        _ => is_transfer_target(&guard, t, &req.account_id),
                    }
            })
            .collect();
        // Merged across organizations, so impose a stable order for
        // pagination rather than relying on per-organization map order.
        // `ListInboundResponsibilityTransfers` models a not-found error, so
        // a named id the caller is not a party to is reported rather than
        // silently returned as an empty page.
        if let Some(id) = &only_id {
            if rows.is_empty() {
                return Err(transfer_not_found(id));
            }
        }
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        let filtered: Vec<Value> = rows.into_iter().map(transfer_payload).collect();
        let (page, token) = paginate_checked(&filtered, next_token.as_deref(), max_results)
            .map_err(|_| invalid_input("Invalid NextToken"))?;
        let mut out = json!({ "ResponsibilityTransfers": page });
        if let Some(t) = token {
            out["NextToken"] = json!(t);
        }
        Ok(AwsResponse::ok_json(out))
    }
}

/// Parse a JSON timestamp that may arrive as an epoch number (seconds,
/// possibly fractional) or an ISO-8601 string.
fn json_to_datetime(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(secs) = v.as_f64() {
        return DateTime::from_timestamp(secs as i64, 0);
    }
    if let Some(s) = v.as_str() {
        return DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc));
    }
    None
}
