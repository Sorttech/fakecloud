//! `OrganizationsService` `handshakes` family — extracted from service.rs by audit-2026-05-19.

use super::*;
use crate::state::target_account_id;

/// The account a handshake names, resolving an EMAIL target back to the
/// account id it encodes. `None` for an address fakecloud cannot map to
/// an account.
fn handshake_target_account(h: &crate::state::Handshake) -> Option<String> {
    target_account_id(&h.target_kind, &h.target_account_id)
}

impl OrganizationsService {
    pub(super) fn accept_handshake(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        self.transition_handshake(req, "ACCEPTED")
    }

    pub(super) fn decline_handshake(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        self.transition_handshake(req, "DECLINED")
    }

    pub(super) fn cancel_handshake(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        self.transition_handshake(req, "CANCELED")
    }

    pub(super) fn transition_handshake(
        &self,
        req: &AwsRequest,
        new_state: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "HandshakeId")?.to_string();
        let mut guard = self.state.write();
        // Handshake transitions are handshake-scoped, not org-scoped, and
        // deliberately so: the account answering an invitation is not yet a
        // member of the inviting organization, so resolving the caller's own
        // organization would never find the handshake. Look it up by id
        // across every organization instead, then let the party gate below
        // decide who may act on it. With no handshake anywhere the id can't
        // be found — these ops don't declare
        // AWSOrganizationsNotInUseException.
        let not_found = || org_error_to_aws(crate::state::OrgError::HandshakeNotFound(id.clone()));
        let (org_id, handshake) = {
            let org = guard.org_of_handshake(&id).ok_or_else(not_found)?;
            let handshake = org.handshakes.get(&id).ok_or_else(not_found)?.clone();
            (org.org_id.clone(), handshake)
        };

        // AcceptHandshake / DeclineHandshake belong to the *target*
        // account; CancelHandshake belongs to the *source* (management)
        // account. Enforce party-correctness so test harnesses catch
        // misuse before AWS would.
        //
        // ENABLE_ALL_FEATURES / APPROVE_ALL_FEATURES handshakes are
        // org-wide and any member account can accept/decline their copy;
        // they don't have a single target_account_id, so we skip the
        // party gate for them.
        let org_wide_action = matches!(
            handshake.action.as_str(),
            "ENABLE_ALL_FEATURES" | "APPROVE_ALL_FEATURES"
        );
        let allowed = if org_wide_action {
            // For org-wide handshakes only require membership -- of the
            // organization that owns the handshake. With several
            // organizations in the process, "any account" would let an
            // outsider resolve a handshake it has nothing to do with.
            guard
                .org_of_account(&req.account_id)
                .is_some_and(|org| org.org_id == org_id)
        } else {
            match new_state {
                "ACCEPTED" | "DECLINED" => {
                    handshake_target_account(&handshake).as_deref() == Some(req.account_id.as_str())
                }
                "CANCELED" => req.account_id == handshake.source_account_id,
                _ => false,
            }
        };
        if !allowed {
            return Err(org_error_to_aws(
                crate::state::OrgError::InvalidHandshakeParty(req.account_id.clone()),
            ));
        }
        // Re-check membership at accept time, not just at invite time: the
        // target may have joined another organization while the invitation
        // sat open, and an account can only ever be in one. Only an INVITE
        // enrolls the target, so only an INVITE can collide — a
        // TRANSFER_RESPONSIBILITY handshake targets another organization's
        // management account by design.
        if new_state == "ACCEPTED" && handshake.action == "INVITE" {
            let target = handshake_target_account(&handshake).unwrap_or_default();
            if let Some(other) = guard.org_of_account(&target) {
                if other.org_id != org_id {
                    return Err(org_error_to_aws(
                        crate::state::OrgError::AccountInAnotherOrganization(target.clone()),
                    ));
                }
            }
        }
        let org = guard
            .org_by_id_mut(&org_id)
            .expect("handshake lookup resolved this organization");
        let updated = org
            .resolve_handshake(&id, new_state)
            .map_err(org_error_to_aws)?;
        Ok(AwsResponse::ok_json(
            json!({ "Handshake": handshake_payload(&updated) }),
        ))
    }

    pub(super) fn describe_handshake(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "HandshakeId")?.to_string();
        let guard = self.state.read();
        // DescribeHandshake is handshake-scoped; with no org the id can't be
        // found. It doesn't declare AWSOrganizationsNotInUseException.
        let handshake = guard
            .org_of_handshake(&id)
            .and_then(|org| org.handshakes.get(&id))
            .ok_or_else(|| {
                org_error_to_aws(crate::state::OrgError::HandshakeNotFound(id.clone()))
            })?;
        // Only the two parties to a handshake may read it. Without this a
        // bystander account could enumerate handshake ids to learn the
        // management account and organization id of an organization it has
        // nothing to do with.
        // An EMAIL target stores the address, so resolve it back to the
        // account it names before comparing. Leaving the gate off entirely
        // would let any account anywhere read any handshake, learning
        // another organization's id and management account.
        let is_party = req.account_id == handshake.source_account_id
            || handshake_target_account(handshake).as_deref() == Some(req.account_id.as_str());
        // AWS documents DescribeHandshake as callable "from any account in
        // the organization", so membership of the organization that owns
        // the handshake is enough. It is only another ORGANIZATION's
        // handshakes that must stay invisible.
        let is_member = guard
            .org_of_handshake(&id)
            .is_some_and(|org| org.accounts.contains_key(&req.account_id));
        if !(is_party || is_member) {
            return Err(org_error_to_aws(crate::state::OrgError::HandshakeNotFound(
                id.clone(),
            )));
        }
        Ok(AwsResponse::ok_json(
            json!({ "Handshake": handshake_payload(handshake) }),
        ))
    }

    pub(super) fn list_handshakes_for_organization(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let filter = parse_handshake_filter(&body)?;
        let (max_results, next_token) = parse_list_pagination(&body)?;

        let guard = self.state.read();
        let org = self.management_org(&guard, &req.account_id)?;
        let filtered: Vec<Value> = org
            .list_handshakes(None)
            .into_iter()
            .filter(|h| handshake_matches_filter(h, &filter))
            .map(|h| handshake_payload(&h))
            .collect();
        let (page, token) = paginate_checked(&filtered, next_token.as_deref(), max_results)
            .map_err(|_| invalid_input("Invalid NextToken"))?;
        let mut body = json!({ "Handshakes": page });
        if let Some(t) = token {
            body["NextToken"] = json!(t);
        }
        Ok(AwsResponse::ok_json(body))
    }
}
