//! `OrganizationsService` `accounts` family — extracted from service.rs by audit-2026-05-19.

use super::*;

impl OrganizationsService {
    pub(super) fn list_accounts(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let guard = self.state.read();
        let org = self.require_member(&guard, &req.account_id)?;
        let accounts: Vec<Value> = org.accounts.values().map(account_payload).collect();
        Ok(AwsResponse::ok_json(json!({ "Accounts": accounts })))
    }

    pub(super) fn list_accounts_for_parent(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let parent_id = required_str(&body, "ParentId")?;
        let guard = self.state.read();
        let org = self.require_member(&guard, &req.account_id)?;
        if parent_id != org.root_id && !org.ous.contains_key(parent_id) {
            return Err(org_error_to_aws(OrgError::ParentNotFound(
                parent_id.to_string(),
            )));
        }
        let accounts: Vec<Value> = org
            .accounts
            .values()
            .filter(|a| a.parent_id == parent_id)
            .map(account_payload)
            .collect();
        Ok(AwsResponse::ok_json(json!({ "Accounts": accounts })))
    }

    pub(super) fn describe_account(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let account_id = required_str(&body, "AccountId")?;
        let guard = self.state.read();
        let org = self.require_member(&guard, &req.account_id)?;
        let account = org
            .accounts
            .get(account_id)
            .ok_or_else(|| org_error_to_aws(OrgError::AccountNotFound(account_id.to_string())))?;
        Ok(AwsResponse::ok_json(
            json!({ "Account": account_payload(account) }),
        ))
    }

    pub(super) fn move_account(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let account_id = required_str(&body, "AccountId")?;
        let source = required_str(&body, "SourceParentId")?;
        let dest = required_str(&body, "DestinationParentId")?;
        let mut guard = self.state.write();
        let org = self.management_org_mut(&mut guard, &req.account_id)?;
        org.move_account(account_id, source, dest)
            .map_err(org_error_to_aws)?;
        Ok(AwsResponse::ok_json(Value::Null))
    }

    pub(super) fn create_account(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let email = required_str(&body, "Email")?.to_string();
        let name = required_str(&body, "AccountName")?.to_string();

        let mut guard = self.state.write();
        let org = self.management_org_mut(&mut guard, &req.account_id)?;
        let status = org.begin_create_account(&email, &name, None);
        let request_id = status.id.clone();
        // Apply create-time Tags to the reserved account id so
        // ListTagsForResource reflects them without a follow-up TagResource.
        // The id is reserved synchronously (before background enrollment), so
        // the tags survive and are queryable immediately (bug-hunt).
        let tags = parse_tags(body.get("Tags"));
        if !tags.is_empty() {
            if let Some(acct_id) = status.account_id.clone() {
                org.set_resource_tags(&acct_id, &tags);
            }
        }
        drop(guard);

        self.spawn_create_account_completion(request_id);

        Ok(AwsResponse::ok_json(json!({
            "CreateAccountStatus": create_account_status_payload(&status),
        })))
    }

    pub(super) fn create_gov_cloud_account(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let email = required_str(&body, "Email")?.to_string();
        let name = required_str(&body, "AccountName")?.to_string();

        let mut guard = self.state.write();
        let org = self.management_org_mut(&mut guard, &req.account_id)?;
        // The GovCloud "paired" id is a 12-digit account id in the
        // GovCloud partition; we mint one alongside the commercial id
        // so callers see both, matching the real AWS response.
        let gov_id = org.next_account_id();
        let status = org.begin_create_account(&email, &name, Some(gov_id));
        let request_id = status.id.clone();
        // Apply create-time Tags to the reserved (primary) account id, mirroring
        // CreateAccount; the id is reserved synchronously (bug-hunt).
        let tags = parse_tags(body.get("Tags"));
        if !tags.is_empty() {
            if let Some(acct_id) = status.account_id.clone() {
                org.set_resource_tags(&acct_id, &tags);
            }
        }
        drop(guard);

        self.spawn_create_account_completion(request_id);

        Ok(AwsResponse::ok_json(json!({
            "CreateAccountStatus": create_account_status_payload(&status),
        })))
    }

    /// Spawn a background tokio task that flips `request_id` from
    /// `IN_PROGRESS` to `SUCCEEDED` after a synthetic 1-2s delay,
    /// enrolling the reserved account id (and GovCloud paired id, if
    /// any) into `state.accounts`. Mirrors the async shape of real
    /// AWS `CreateAccount` so SDK callers can observe both phases.
    pub(super) fn spawn_create_account_completion(&self, request_id: String) {
        let state = self.state.clone();
        let store = self.snapshot_store.clone();
        let lock = self.snapshot_lock.clone();
        let hooks = self.change_hooks.clone();
        let delay = {
            let mut rng = rand::thread_rng();
            let span = CREATE_ACCOUNT_MAX_DELAY.saturating_sub(CREATE_ACCOUNT_MIN_DELAY);
            let jitter_millis = if span.is_zero() {
                0
            } else {
                rng.gen_range(0..=span.as_millis() as u64)
            };
            CREATE_ACCOUNT_MIN_DELAY + Duration::from_millis(jitter_millis)
        };
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let completed = {
                let mut guard = state.write();
                // Request ids are globally unique, so the owning
                // organization is whichever one holds this request.
                match guard.org_of_create_account_request_mut(&request_id) {
                    Some(org) => {
                        org.complete_create_account(&request_id);
                        true
                    }
                    None => false,
                }
            };
            if completed {
                super::save_organizations_snapshot(&state, store, &lock).await;
                // The account only joins the organization here, so this is
                // where StackSets auto-deployment gets to see it.
                hooks.fire_if_membership_changed(&state).await;
            }
        });
    }

    pub(super) fn describe_create_account_status(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let request_id = required_str(&body, "CreateAccountRequestId")?.to_string();

        let guard = self.state.read();
        let org = self.require_member(&guard, &req.account_id)?;
        let status = org.describe_create_account(&request_id).ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "CreateAccountStatusNotFoundException",
                format!("Create account status with id {request_id} was not found."),
            )
        })?;
        Ok(AwsResponse::ok_json(json!({
            "CreateAccountStatus": create_account_status_payload(&status),
        })))
    }

    pub(super) fn list_create_account_status(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let states: Vec<String> = body
            .get("States")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        // AWS caps MaxResults at 20 for ListCreateAccountStatus and
        // defaults to 20 when unset. Reject out-of-range values with
        // InvalidInputException so callers see the same wire error
        // they would from real AWS, instead of silently clamping.
        let max_results = match body.get("MaxResults") {
            None | Some(Value::Null) => 20,
            Some(v) => {
                let n = v.as_u64().ok_or_else(|| {
                    AwsServiceError::aws_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidInputException",
                        "MaxResults must be a positive integer between 1 and 20.",
                    )
                })?;
                if !(1..=20).contains(&n) {
                    return Err(AwsServiceError::aws_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidInputException",
                        "MaxResults must be between 1 and 20.",
                    ));
                }
                n as usize
            }
        };
        // NextToken must round-trip a token we previously emitted.
        // Reject anything we didn't mint (non-numeric, negative, etc.)
        // up front so callers learn about a typo instead of silently
        // re-reading page 1.
        let next_token = match body.get("NextToken") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let s = v.as_str().ok_or_else(|| {
                    AwsServiceError::aws_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidInputException",
                        "NextToken must be a string.",
                    )
                })?;
                // Tokens we mint are positive offset integers (see
                // fakecloud_core::pagination::paginate).
                if s.parse::<usize>().is_err() {
                    return Err(AwsServiceError::aws_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidInputException",
                        "NextToken is not a valid pagination token.",
                    ));
                }
                Some(s.to_string())
            }
        };

        let guard = self.state.read();
        let org = self.require_member(&guard, &req.account_id)?;
        let filtered: Vec<Value> = org
            .create_account_requests
            .values()
            .filter(|s| states.is_empty() || states.iter().any(|st| st == &s.state))
            .map(create_account_status_payload)
            .collect();
        let (page, token) = paginate_checked(&filtered, next_token.as_deref(), max_results)
            .map_err(|_| invalid_input("Invalid NextToken"))?;
        let mut body = json!({ "CreateAccountStatuses": page });
        if let Some(t) = token {
            body["NextToken"] = json!(t);
        }
        Ok(AwsResponse::ok_json(body))
    }

    pub(super) fn close_account(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let target = required_str(&body, "AccountId")?.to_string();

        let mut guard = self.state.write();
        let org = self.management_org_mut(&mut guard, &req.account_id)?;
        org.close_account(&target).map_err(org_error_to_aws)?;
        Ok(AwsResponse::ok_json(json!({})))
    }

    pub(super) fn remove_account_from_organization(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let target = required_str(&body, "AccountId")?.to_string();

        let mut guard = self.state.write();
        let org = self.management_org_mut(&mut guard, &req.account_id)?;
        org.remove_account(&target).map_err(org_error_to_aws)?;
        Ok(AwsResponse::ok_json(json!({})))
    }

    /// `LeaveOrganization` removes the *calling* member account from its
    /// organization. The management account cannot leave its own org
    /// (it must `DeleteOrganization` instead), and a caller that isn't a
    /// member of any org gets `AccountNotFoundException`.
    pub(super) fn leave_organization(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let mut guard = self.state.write();
        // Resolve the caller's OWN organization: an account can only leave
        // the one it is actually in. AWS answers a caller that belongs to no
        // organization with `AWSOrganizationsNotInUseException`, the same
        // answer every other op gives, rather than `AccountNotFoundException`.
        let org = guard
            .org_of_account_mut(&req.account_id)
            .ok_or_else(organizations_not_in_use)?;
        if org.is_management(&req.account_id) {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "MasterCannotLeaveOrganizationException",
                "The management account in an organization cannot be removed; \
                 delete the organization instead.",
            ));
        }
        org.remove_account(&req.account_id)
            .map_err(org_error_to_aws)?;
        Ok(AwsResponse::ok_json(json!({})))
    }

    pub(super) fn invite_account_to_organization(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let target_obj = body.get("Target").ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "InvalidInputException",
                "Target is required",
            )
        })?;
        let kind = target_obj
            .get("Type")
            .and_then(|v| v.as_str())
            .unwrap_or("ACCOUNT");
        let id = target_obj
            .get("Id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AwsServiceError::aws_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidInputException",
                    "Target.Id is required",
                )
            })?
            .to_string();
        // Validate the id against the declared kind. AWS rejects a
        // mismatch with InvalidInputException; accepting one here would
        // open a handshake keyed by a string no caller can ever
        // authenticate as, which sits OPEN forever.
        validate_invite_target(kind, &id)?;
        let notes = body
            .get("Notes")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let target_email = if kind == "EMAIL" {
            Some(id.clone())
        } else {
            None
        };

        let mut guard = self.state.write();
        let org_id = self
            .management_org_mut(&mut guard, &req.account_id)?
            .org_id
            .clone();
        // An account belongs to at most one organization, so an invitation
        // to an account another organization already holds must be rejected
        // here rather than quietly opening a handshake that could never be
        // accepted. A target that is already OUR member falls through to
        // `invite_account`, which reports it as such.
        // Applies to an EMAIL target too, once it resolves to an account
        // fakecloud knows -- otherwise the invite would open a handshake
        // that could never be accepted.
        if let Some(target) = crate::state::target_account_id(kind, &id) {
            if let Some(other) = guard.org_of_account(&target) {
                if other.org_id != org_id {
                    return Err(org_error_to_aws(
                        crate::state::OrgError::AccountInAnotherOrganization(target),
                    ));
                }
            }
        }
        let org = guard
            .org_by_id_mut(&org_id)
            .expect("management gate resolved this organization");
        let handshake = org
            .invite_account(&req.account_id, kind, &id, target_email, notes)
            .map_err(org_error_to_aws)?;
        Ok(AwsResponse::ok_json(
            json!({ "Handshake": handshake_payload(&handshake) }),
        ))
    }

    pub(super) fn list_handshakes_for_account(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let filter = parse_handshake_filter(&body)?;
        let (max_results, next_token) = parse_list_pagination(&body)?;

        let guard = self.state.read();
        // ListHandshakesForAccount is scoped to the calling account, not to
        // an organization — a caller in no org simply has no handshakes.
        // (The op doesn't even declare AWSOrganizationsNotInUseException.)
        // It spans every organization on purpose: the invitations a
        // standalone account most wants to list are the ones held by the
        // organizations inviting it, none of which it belongs to yet.
        let mut filtered: Vec<Value> = guard
            .iter()
            .flat_map(|org| org.list_handshakes(Some(&req.account_id)))
            .filter(|h| handshake_matches_filter(h, &filter))
            .map(|h| handshake_payload(&h))
            .collect();
        // `list_handshakes` sorts within an organization; re-sort so the
        // merged result is stable and paginates consistently.
        filtered.sort_by(|a, b| {
            a.get("Id")
                .and_then(Value::as_str)
                .cmp(&b.get("Id").and_then(Value::as_str))
        });
        let (page, token) = paginate_checked(&filtered, next_token.as_deref(), max_results)
            .map_err(|_| invalid_input("Invalid NextToken"))?;
        let mut body = json!({ "Handshakes": page });
        if let Some(t) = token {
            body["NextToken"] = json!(t);
        }
        Ok(AwsResponse::ok_json(body))
    }

    pub(super) fn list_delegated_services_for_account(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let account_id = required_str(&body, "AccountId")?.to_string();
        let (max_results, next_token) = parse_list_pagination(&body)?;
        let guard = self.state.read();
        let org = self.management_org(&guard, &req.account_id)?;
        let entries: Vec<Value> = org
            .list_delegated_services_for_account(&account_id)
            .into_iter()
            .map(|(svc, enabled_at)| {
                json!({
                    "ServicePrincipal": svc,
                    "DelegationEnabledDate": enabled_at.timestamp() as f64,
                })
            })
            .collect();
        let (page, token) = paginate_checked(&entries, next_token.as_deref(), max_results)
            .map_err(|_| invalid_input("Invalid NextToken"))?;
        let mut body = json!({ "DelegatedServices": page });
        if let Some(t) = token {
            body["NextToken"] = json!(t);
        }
        Ok(AwsResponse::ok_json(body))
    }
}

/// Check `Target.Id` against the declared `Target.Type`.
///
/// An `ACCOUNT` target must be a 12-digit account id. An `EMAIL` target
/// must be an address, and must additionally name an account fakecloud
/// can resolve — an address it has never minted identifies no account,
/// so the invitation could never be accepted and would sit OPEN
/// forever rather than failing where the caller can see it.
fn validate_invite_target(kind: &str, id: &str) -> Result<(), AwsServiceError> {
    match kind {
        "ACCOUNT" => {
            if id.len() == 12 && id.chars().all(|c| c.is_ascii_digit()) {
                Ok(())
            } else {
                Err(invalid_input(
                    "Target.Id must be a 12-digit account id when Target.Type is ACCOUNT",
                ))
            }
        }
        "EMAIL" => {
            if !id.contains('@') {
                return Err(invalid_input(
                    "Target.Id must be an email address when Target.Type is EMAIL",
                ));
            }
            if crate::state::target_account_id("EMAIL", id).is_none() {
                return Err(invalid_input(&format!(
                    "No account is registered for {id}. fakecloud resolves an EMAIL target \
                     only for addresses it minted (<account-id>@example.com); invite the \
                     account by id instead."
                )));
            }
            Ok(())
        }
        other => Err(invalid_input(&format!(
            "Target.Type must be one of [ACCOUNT, EMAIL], got {other}"
        ))),
    }
}
