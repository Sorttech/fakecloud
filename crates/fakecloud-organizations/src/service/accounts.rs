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
        // The address check runs in the completion tick, not here: AWS
        // reports a duplicate as a FAILED request rather than a
        // synchronous error, and running a registry-wide check here would
        // also tell any caller whether an address is registered in an
        // organization it has nothing to do with.
        let org_id = self
            .management_org_mut(&mut guard, &req.account_id)?
            .org_id
            .clone();
        // Mint from the registry: an account id names one account
        // process-wide, so a per-organization check could hand out an id
        // another organization already owns.
        let new_account_id = guard.next_account_id();
        let org = guard
            .org_by_id_mut(&org_id)
            .expect("management gate resolved this organization");
        let status = org.begin_create_account(&email, &name, new_account_id, None);
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
        // Authorize before the registry-wide address check, as above.
        let org_id = self
            .management_org_mut(&mut guard, &req.account_id)?
            .org_id
            .clone();
        // The GovCloud "paired" id is a 12-digit account id in the
        // GovCloud partition; we mint one alongside the commercial id
        // so callers see both, matching the real AWS response. Both come
        // from the registry so neither can collide with another
        // organization's account.
        let new_account_id = guard.next_account_id();
        // Exclude the id just minted: it is not recorded anywhere yet, so
        // a plain second call could hand back the same one.
        let gov_id = guard.next_account_id_besides(&[new_account_id.as_str()]);
        let org = guard
            .org_by_id_mut(&org_id)
            .expect("management gate resolved this organization");
        let status = org.begin_create_account(&email, &name, new_account_id, Some(gov_id));
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
            let mut failed = false;
            let completed = {
                let mut guard = state.write();
                // An address already in use fails the request rather than
                // the call: `CreateAccount` models no synchronous error for
                // it, and clients hand back a request id to poll. AWS
                // reports it as FAILED with EMAIL_ALREADY_EXISTS.
                let taken = guard
                    .org_of_create_account_request(&request_id)
                    .and_then(|org| org.create_account_requests.get(&request_id))
                    .and_then(|req| req.pending_email.clone())
                    .is_some_and(|email| {
                        // The address must be free, and must not be the
                        // synthetic form reserved for a different id.
                        let spelled_for_other = guard
                            .org_of_create_account_request(&request_id)
                            .and_then(|org| org.create_account_requests.get(&request_id))
                            .and_then(|req| req.account_id.clone())
                            .is_some_and(|mine| {
                                crate::state::OrganizationsRegistry::email_reserved_for_other(
                                    &email, &mine,
                                )
                            });
                        spelled_for_other || guard.email_in_use_besides(&email, &request_id)
                    });
                // Request ids are globally unique, so the owning
                // organization is whichever one holds this request.
                match guard.org_of_create_account_request_mut(&request_id) {
                    Some(org) if taken => {
                        org.fail_create_account(&request_id, "EMAIL_ALREADY_EXISTS");
                        failed = true;
                        false
                    }
                    Some(org) => {
                        org.complete_create_account(&request_id);
                        true
                    }
                    None => false,
                }
            };
            if completed || failed {
                // FAILED is durable too: the request was persisted as
                // IN_PROGRESS, so a restart would otherwise re-arm it and
                // a poller would watch it go FAILED -> IN_PROGRESS ->
                // FAILED.
                super::save_organizations_snapshot(&state, store, &lock).await;
            }
            if completed {
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
    /// member of any org gets `AWSOrganizationsNotInUseException` — the
    /// op takes no AccountId, and with several organizations in the
    /// process that is also the non-leaking answer.
    pub(super) fn leave_organization(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let mut guard = self.state.write();
        // Resolve the caller's OWN organization: an account can only leave
        // the one it is actually in. The op takes no AccountId, so AWS
        // answers a caller that belongs to no organization with
        // `AWSOrganizationsNotInUseException` -- also the non-leaking answer
        // now that several organizations can coexist.
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
        // `HandshakeParty.Type` is modeled required. Defaulting it meant a
        // caller that omitted it had its id validated against a type it
        // never declared.
        let kind = target_obj
            .get("Type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_input("Target.Type is required"))?;
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
        // Applies to an EMAIL target once it resolves, but resolution here
        // is deliberately scoped to the caller's own organization:
        // resolving registry-wide would let the caller read a foreign
        // account id back out of the error below. An address registered
        // in ANOTHER organization therefore slips past this guard and is
        // caught at accept time instead, which costs a handshake that
        // sits OPEN until it expires -- the price of not answering "whose
        // account is this?" to anyone who asks.
        if let Some(target) = guard.resolve_target_account(kind, &id, &org_id) {
            if guard.claimed_by_other_org(&target, &org_id).is_some() {
                return Err(org_error_to_aws(
                    crate::state::OrgError::AccountInAnotherOrganization(target),
                ));
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
            .flat_map(|org| org.list_handshakes())
            .filter(|h| {
                // "Associated with the account of the requesting user"
                // includes the invitations it SENT, not only those
                // addressed to it. Cross-organization isolation is
                // unaffected: the source is always a member of the
                // organization that owns the handshake.
                h.source_account_id == req.account_id
                    || guard.account_matches_target(
                        &h.target_kind,
                        &h.target_account_id,
                        &req.account_id,
                    )
            })
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

/// Check `Target.Id` against the declared `Target.Type`. AWS accepts
/// `ACCOUNT` and `EMAIL` only.
///
/// An `ACCOUNT` target must be a 12-digit account id. An `EMAIL` target
/// must be an address, and any address is accepted — inviting the
/// account owner's real address is AWS's primary flow. fakecloud can
/// only resolve one it minted itself (`<account-id>@example.com`) back
/// to an account, so an external address yields a handshake its source
/// can read and cancel but that no caller can prove it is the target
/// of; it stays OPEN until it expires, mirroring AWS before the owner
/// acts on the emailed link.
pub(super) fn validate_invite_target(kind: &str, id: &str) -> Result<(), AwsServiceError> {
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
            // AWS's primary invite flow names the account owner's real
            // address, so any address is accepted. fakecloud can only
            // resolve one it minted itself (`<account-id>@example.com`)
            // back to an account, so an external address yields a handshake
            // its source can read and cancel but that no caller can prove
            // it is the target of -- same as AWS before the owner acts on
            // the emailed link.
            if !id.contains('@') {
                return Err(invalid_input(
                    "Target.Id must be an email address when Target.Type is EMAIL",
                ));
            }
            Ok(())
        }
        other => Err(invalid_input(&format!(
            "Target.Type must be one of [ACCOUNT, EMAIL], got {other}"
        ))),
    }
}
