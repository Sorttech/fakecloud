//! IPAM internet-registry associations and the routing policy registrations
//! (RPKI route origin authorizations) published through them.
//!
//! An association ties an IPAM to one Regional Internet Registry. Registrations
//! hang off it, keyed by CIDR, and every change to them produces a delta: the
//! deltas are the audit trail, so they outlive the registrations they describe.
//!
//! A registration publishes through the association's RPKI service, and
//! `EnableIpamInternetRegistryAssociation` is what establishes that service --
//! the model says "after enabling, you can create Route Origin Authorizations
//! (ROAs)". So an association still in `pending-enable` publishes nothing.
//!
//! `ClientToken` is an idempotency token on every mutating operation here. The
//! association records the tokens it has served, keyed by operation, so a retry
//! replays the delta (or the association) the first call produced instead of
//! failing on the change that call already made.

use chrono::Utc;

use fakecloud_aws::ec2query::{ec2_elem, ec2_list};
use fakecloud_aws::xml::xml_escape;
use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::service::Ec2Service;
use crate::service_helpers::{
    filter_value_matches, gen_id, indexed_list, invalid_parameter_value, not_found, paginate,
    parse_filters, require, validate_enum, validate_max_results, Filter,
};
use crate::state::{
    Ec2State, IpamInternetRegistryAssociation, IpamRoutingPolicyRegistration,
    IpamRoutingPolicyRegistrationDelta, Tag,
};

const RIRS: &[&str] = &["ripe", "apnic", "arin", "lacnic"];

/// `MaxResults` and `NextToken` for the module's paginated reads.
/// `IpamMaxResults` carries `@range 5..1000`; the token is the next offset
/// [`paginate`] hands back.
fn pagination(req: &AwsRequest) -> Result<(Option<usize>, Option<String>), AwsServiceError> {
    validate_max_results(&req.query_params, 5, 1000)?;
    let max_results = req
        .query_params
        .get("MaxResults")
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<usize>().ok());
    let next_token = req
        .query_params
        .get("NextToken")
        .filter(|v| !v.is_empty())
        .cloned();
    if let Some(t) = &next_token {
        if t.parse::<usize>().is_err() {
            return Err(invalid_parameter_value(format!(
                "Invalid value '{t}' for NextToken"
            )));
        }
    }
    Ok((max_results, next_token))
}

/// Page a rendered item list into the operation's set element, plus the
/// `nextToken` every one of these results models.
fn paged_response(
    action: &'static str,
    req: &AwsRequest,
    wrapper: &str,
    items: &[String],
    page: (Option<usize>, Option<String>),
) -> AwsResponse {
    let (max_results, next_token) = page;
    let (items, token) = paginate(items, next_token.as_deref(), max_results);
    Ec2Service::respond(
        action,
        &req.request_id,
        &format!(
            "{}{}",
            ec2_list(wrapper, &items),
            token.map(|t| ec2_elem("nextToken", &t)).unwrap_or_default()
        ),
    )
}

/// Apply a request's `Filter.N` set: values within a filter are OR'd and the
/// filters themselves are AND'd, which is what AWS does. `candidates` maps a
/// filter name to the values an item offers under it; `None` means the name is
/// not one this operation supports, and an unsupported name matches nothing --
/// the same way the rest of the EC2 describes treat one.
fn matches_filters(filters: &[Filter], candidates: impl Fn(&str) -> Option<Vec<String>>) -> bool {
    filters.iter().all(|f| match candidates(&f.name) {
        Some(values) => f
            .values
            .iter()
            .any(|want| values.iter().any(|have| filter_value_matches(want, have))),
        None => false,
    })
}

fn region_of(req: &AwsRequest) -> String {
    if req.region.is_empty() {
        "us-east-1".to_string()
    } else {
        req.region.clone()
    }
}

fn dry_run(req: &AwsRequest) -> bool {
    req.query_params
        .get("DryRun")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn client_token(req: &AwsRequest) -> Option<String> {
    req.query_params
        .get("ClientToken")
        .filter(|v| !v.is_empty())
        .cloned()
}

/// Idempotency records are scoped to the operation, so a token a caller reuses
/// across two different calls cannot replay the other one's result.
fn token_key(action: &str, token: &str) -> String {
    format!("{action}:{token}")
}

/// The delta an earlier call under this idempotency token produced, if any. A
/// retry replays it rather than applying the change a second time (or failing
/// on the state the first call left behind).
fn replay_delta(
    a: &IpamInternetRegistryAssociation,
    action: &str,
    token: Option<&str>,
) -> Option<IpamRoutingPolicyRegistrationDelta> {
    let recorded = a.client_tokens.get(&token_key(action, token?))?;
    a.deltas.iter().find(|d| &d.delta_id == recorded).cloned()
}

fn record_client_token(
    a: &mut IpamInternetRegistryAssociation,
    action: &str,
    token: Option<&str>,
    result_id: &str,
) {
    if let Some(token) = token {
        a.client_tokens
            .insert(token_key(action, token), result_id.to_string());
    }
}

/// Parse an RFC 3339 time bound, rejecting a malformed one rather than letting
/// a byte comparison silently filter everything out.
fn parse_time_bound(
    req: &AwsRequest,
    key: &str,
) -> Result<Option<chrono::DateTime<Utc>>, AwsServiceError> {
    match req.query_params.get(key).filter(|v| !v.is_empty()) {
        Some(v) => chrono::DateTime::parse_from_rfc3339(v)
            .map(|t| Some(t.with_timezone(&Utc)))
            .map_err(|_| invalid_parameter_value(format!("Invalid value '{v}' for {key}"))),
        None => Ok(None),
    }
}

fn delta_time(d: &IpamRoutingPolicyRegistrationDelta) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(&d.created_at)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// The prefix length of a CIDR, for comparing a ROA's MaxLength against the
/// prefix it covers.
fn cidr_prefix_len(cidr: &str) -> Option<i64> {
    cidr.split_once('/')
        .and_then(|(_, len)| len.parse::<i64>().ok())
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn association_not_found(id: &str) -> AwsServiceError {
    not_found("InvalidIpamInternetRegistryAssociationId.NotFound", id)
}

fn get_association<'a>(
    state: &'a mut Ec2State,
    id: &str,
) -> Result<&'a mut IpamInternetRegistryAssociation, AwsServiceError> {
    state
        .ipam_ir_associations
        .get_mut(id)
        .ok_or_else(|| association_not_found(id))
}

/// A registration is a ROA published through the association's RPKI service,
/// and that service only exists once `EnableIpamInternetRegistryAssociation`
/// has been called. Publishing through an association still in
/// `pending-enable` would make that operation decorative.
fn require_enabled(a: &IpamInternetRegistryAssociation) -> Result<(), AwsServiceError> {
    if a.state == "enable-complete" {
        return Ok(());
    }
    Err(AwsServiceError::aws_error(
        http::StatusCode::BAD_REQUEST,
        "IncorrectState",
        format!(
            "The internet registry association '{}' is in state '{}' and must be enabled before \
             it can publish routing policy registrations",
            a.id, a.state
        ),
    ))
}

fn association_xml(a: &IpamInternetRegistryAssociation, owner: &str, tags: &[Tag]) -> String {
    let mut s = String::new();
    s.push_str(&ec2_elem("ownerId", owner));
    s.push_str(&ec2_elem("ipamInternetRegistryAssociationId", &a.id));
    s.push_str(&ec2_elem(
        "ipamInternetRegistryAssociationArn",
        &format!(
            "arn:aws:ec2::{owner}:ipam-internet-registry-association/{}",
            a.id
        ),
    ));
    s.push_str(&ec2_elem("ipamId", &a.ipam_id));
    s.push_str(&ec2_elem("ipamRegion", &a.region));
    s.push_str(&ec2_elem("rir", &a.rir));
    s.push_str(&ec2_elem("organizationHandle", &a.organization_handle));
    if let Some(d) = &a.description {
        s.push_str(&ec2_elem("description", d));
    }
    s.push_str(&ec2_elem("state", &a.state));
    if let Some(x) = &a.child_request_xml {
        s.push_str(&ec2_elem("childRequestXml", x));
    }
    if !tags.is_empty() {
        s.push_str(&super::tags::tag_set_xml(tags));
    }
    s
}

fn association_matches(
    a: &IpamInternetRegistryAssociation,
    owner: &str,
    tags: &[Tag],
    filters: &[Filter],
) -> bool {
    matches_filters(filters, |name| match name {
        "ipam-internet-registry-association-id" => Some(vec![a.id.clone()]),
        "ipam-id" => Some(vec![a.ipam_id.clone()]),
        "ipam-region" => Some(vec![a.region.clone()]),
        "rir" => Some(vec![a.rir.clone()]),
        "organization-handle" => Some(vec![a.organization_handle.clone()]),
        "state" => Some(vec![a.state.clone()]),
        "owner-id" => Some(vec![owner.to_string()]),
        "tag-key" => Some(tags.iter().map(|t| t.key.clone()).collect()),
        "tag-value" => Some(tags.iter().map(|t| t.value.clone()).collect()),
        other => other.strip_prefix("tag:").map(|key| {
            tags.iter()
                .filter(|t| t.key == key)
                .map(|t| t.value.clone())
                .collect()
        }),
    })
}

fn delta_xml(d: &IpamRoutingPolicyRegistrationDelta) -> String {
    let mut s = String::new();
    s.push_str(&ec2_elem("deltaId", &d.delta_id));
    s.push_str(&ec2_elem("deltaJson", &d.delta_json));
    s.push_str(&ec2_elem("state", &d.state));
    if let Some(m) = &d.state_message {
        s.push_str(&ec2_elem("stateMessage", m));
    }
    s
}

fn registration_xml(r: &IpamRoutingPolicyRegistration) -> String {
    let mut s = String::new();
    s.push_str(&ec2_elem("cidr", &r.cidr));
    let asns: Vec<String> = r.asns.iter().map(|a| ec2_elem("item", a)).collect();
    if !asns.is_empty() {
        s.push_str(&format!("<asnSet>{}</asnSet>", asns.join("")));
    }
    if let Some(p) = r.permit_more_specific_announcements {
        s.push_str(&format!(
            "<permitMoreSpecificAnnouncements>{p}</permitMoreSpecificAnnouncements>"
        ));
    }
    if let Some(m) = r.max_length {
        s.push_str(&format!("<maxLength>{m}</maxLength>"));
    }
    if let Some(d) = &r.description {
        s.push_str(&ec2_elem("description", d));
    }
    s.push_str(&ec2_elem("latestDeltaId", &r.latest_delta_id));
    s.push_str(&ec2_elem("state", &r.state));
    s
}

/// Record a delta against an association and return its id. Deltas publish
/// immediately here: there is no RIR round trip to wait on.
fn push_delta(a: &mut IpamInternetRegistryAssociation, delta_json: String) -> String {
    let delta = IpamRoutingPolicyRegistrationDelta {
        delta_id: gen_id("ipam-delta"),
        delta_json,
        state: "published".to_string(),
        state_message: None,
        created_at: now_rfc3339(),
    };
    let id = delta.delta_id.clone();
    a.deltas.push(delta);
    id
}

fn delta_response(
    action: &'static str,
    req: &AwsRequest,
    d: &IpamRoutingPolicyRegistrationDelta,
) -> AwsResponse {
    Ec2Service::respond(
        action,
        &req.request_id,
        &format!(
            "<ipamRoutingPolicyRegistrationDelta>{}</ipamRoutingPolicyRegistrationDelta>",
            delta_xml(d)
        ),
    )
}

// ---- associations ----

pub(crate) fn create_ipam_internet_registry_association(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let ipam_id = require(&req.query_params, "IpamId")?;
    let rir = require(&req.query_params, "Rir")?;
    let organization_handle = require(&req.query_params, "OrganizationHandle")?;
    validate_enum(&req.query_params, "Rir", RIRS)?;
    let token = client_token(req);

    let owner = req.account_id.clone();
    let region = region_of(req);
    let mut accounts = svc.state.write();
    let state = accounts.get_or_create(&req.account_id);
    if !state.ipams.contains_key(&ipam_id) {
        return Err(not_found("InvalidIpamId.NotFound", &ipam_id));
    }
    // A DryRun validates the request -- including that the IPAM exists, which
    // is exactly the failure a dry run is for -- and changes nothing.
    if dry_run(req) {
        return Ok(Ec2Service::respond(
            "CreateIpamInternetRegistryAssociation",
            &req.request_id,
            "",
        ));
    }
    // A retry that carries the original token gets the original association
    // back rather than a second one for the same registry.
    if let Some(token) = &token {
        if let Some(existing) = state
            .ipam_ir_associations
            .values()
            .find(|a| a.client_token.as_deref() == Some(token.as_str()))
        {
            let tags = state.tags.get(&existing.id).cloned().unwrap_or_default();
            return Ok(Ec2Service::respond(
                "CreateIpamInternetRegistryAssociation",
                &req.request_id,
                &format!(
                    "<ipamInternetRegistryAssociation>{}</ipamInternetRegistryAssociation>",
                    association_xml(existing, &owner, &tags)
                ),
            ));
        }
    }

    let id = gen_id("ipam-ir-assoc");
    let association = IpamInternetRegistryAssociation {
        id: id.clone(),
        ipam_id,
        region,
        rir,
        organization_handle,
        description: req.query_params.get("Description").cloned(),
        // The association exists but cannot publish until it is enabled
        // against the registry's RPKI service.
        state: "pending-enable".to_string(),
        child_request_xml: None,
        registrations: Default::default(),
        deltas: Vec::new(),
        client_token: token,
        client_tokens: Default::default(),
    };
    let tags = {
        crate::service::tags::apply_tag_specifications(
            state,
            &req.query_params,
            &id,
            "ipam-internet-registry-association",
        );
        state.tags.get(&id).cloned().unwrap_or_default()
    };
    state.ipam_ir_associations.insert(id, association.clone());
    Ok(Ec2Service::respond(
        "CreateIpamInternetRegistryAssociation",
        &req.request_id,
        &format!(
            "<ipamInternetRegistryAssociation>{}</ipamInternetRegistryAssociation>",
            association_xml(&association, &owner, &tags)
        ),
    ))
}

pub(crate) fn enable_ipam_internet_registry_association(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    let rpki_version = require(&req.query_params, "RpkiVersion")?;
    let service_uri = require(&req.query_params, "ServiceUri")?;
    let child_handle = require(&req.query_params, "ChildHandle")?;
    let parent_handle = require(&req.query_params, "ParentHandle")?;
    let parent_bpki_ta = require(&req.query_params, "ParentBpkiTa")?;
    let token = client_token(req);

    let owner = req.account_id.clone();
    let mut accounts = svc.state.write();
    let state = accounts.get_or_create(&req.account_id);
    let tags = state.tags.get(&id).cloned().unwrap_or_default();
    let a = get_association(state, &id)?;
    // A DryRun validates the request -- including that the association exists
    // -- and changes nothing, matching how the rest of EC2 treats one.
    if dry_run(req) {
        return Ok(Ec2Service::respond(
            "EnableIpamInternetRegistryAssociation",
            &req.request_id,
            "",
        ));
    }
    // A retry under the original token reports the association the first call
    // enabled, leaving the child request it already issued alone.
    let replaying = token.as_deref().is_some_and(|t| {
        a.client_tokens
            .contains_key(&token_key("EnableIpamInternetRegistryAssociation", t))
    });
    if !replaying {
        // The child request is the RPKI provisioning document the registry
        // needs; it is what the caller takes to the RIR to finish setup. Every
        // value here is caller-supplied and lands in an attribute value or in
        // element text, so it is entity-escaped as it goes in: the response
        // escapes the blob as a whole, so an unescaped `&` or `"` would come
        // back looking fine and only break when the RIR parses the document.
        a.child_request_xml = Some(format!(
            "<publisher_request version=\"{}\" \
             service_uri=\"{}\" \
             child_handle=\"{}\" \
             parent_handle=\"{}\">\
             <parent_bpki_ta>{}</parent_bpki_ta>\
             </publisher_request>",
            xml_escape(&rpki_version),
            xml_escape(&service_uri),
            xml_escape(&child_handle),
            xml_escape(&parent_handle),
            xml_escape(&parent_bpki_ta),
        ));
        a.state = "enable-complete".to_string();
        let association_id = a.id.clone();
        record_client_token(
            a,
            "EnableIpamInternetRegistryAssociation",
            token.as_deref(),
            &association_id,
        );
    }
    let body = format!(
        "<ipamInternetRegistryAssociation>{}</ipamInternetRegistryAssociation>",
        association_xml(a, &owner, &tags)
    );
    Ok(Ec2Service::respond(
        "EnableIpamInternetRegistryAssociation",
        &req.request_id,
        &body,
    ))
}

pub(crate) fn delete_ipam_internet_registry_association(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    let owner = req.account_id.clone();
    let mut accounts = svc.state.write();
    let state = accounts.get_or_create(&req.account_id);
    // The model is explicit that the registrations have to be removed before
    // the association can go, and deleting one that still publishes ROAs would
    // orphan them. EC2 reports a delete blocked by what depends on the
    // resource as `DependencyViolation`; the model declares no errors of its
    // own for this operation.
    if !state
        .ipam_ir_associations
        .get(&id)
        .ok_or_else(|| association_not_found(&id))?
        .registrations
        .is_empty()
    {
        return Err(AwsServiceError::aws_error(
            http::StatusCode::BAD_REQUEST,
            "DependencyViolation",
            format!(
                "The internet registry association '{id}' still has routing policy \
                 registrations; remove them before deleting it"
            ),
        ));
    }
    // A DryRun validates the request -- including that the association exists
    // and that nothing depends on it -- and changes nothing, matching how the
    // rest of EC2 treats one.
    if dry_run(req) {
        return Ok(Ec2Service::respond(
            "DeleteIpamInternetRegistryAssociation",
            &req.request_id,
            "",
        ));
    }
    let tags = state.tags.get(&id).cloned().unwrap_or_default();
    let mut association = state
        .ipam_ir_associations
        .remove(&id)
        .ok_or_else(|| association_not_found(&id))?;
    // The response reports the association in its terminal state.
    association.state = "delete-complete".to_string();
    state.tags.remove(&id);
    Ok(Ec2Service::respond(
        "DeleteIpamInternetRegistryAssociation",
        &req.request_id,
        &format!(
            "<ipamInternetRegistryAssociation>{}</ipamInternetRegistryAssociation>",
            association_xml(&association, &owner, &tags)
        ),
    ))
}

pub(crate) fn describe_ipam_internet_registry_associations(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let page = pagination(req)?;
    let ids = indexed_list(&req.query_params, "IpamInternetRegistryAssociationId");
    let filters = parse_filters(&req.query_params);
    let owner = req.account_id.clone();
    let accounts = svc.state.read();
    let mut items = Vec::new();
    if let Some(state) = accounts.get(&req.account_id) {
        for (id, a) in &state.ipam_ir_associations {
            if !ids.is_empty() && !ids.contains(id) {
                continue;
            }
            let tags = state.tags.get(id).cloned().unwrap_or_default();
            if !association_matches(a, &owner, &tags, &filters) {
                continue;
            }
            items.push(association_xml(a, &owner, &tags));
        }
    }
    Ok(paged_response(
        "DescribeIpamInternetRegistryAssociations",
        req,
        "ipamInternetRegistryAssociationSet",
        &items,
        page,
    ))
}

// ---- routing policy registrations ----

/// Which of the three registration writes is being applied. Create rejects a
/// CIDR that is already registered and Modify one that is not; a batch `add`
/// entry is documented to "create, update, or delete" and so accepts either.
#[derive(Clone, Copy, PartialEq)]
enum RegistrationWrite {
    Create,
    Modify,
    Upsert,
}

/// The fields one registration change carries. The single operations parse
/// them from indexed query parameters and a batch entry parses them from its
/// JSON object, so both reach the same validation and the same writer.
struct RegistrationChange {
    cidr: String,
    asns: Vec<String>,
    permit_more_specific_announcements: Option<bool>,
    max_length: Option<i64>,
    description: Option<String>,
}

/// `IpamRoutingPolicyRegistrationMaxLength` carries `@range 0..48`, and the
/// member documents that it must not be shorter than the CIDR's own prefix
/// length -- a ROA that authorizes less than the prefix it covers announces
/// nothing. Both bounds hold wherever the change came from.
fn validate_change(change: &RegistrationChange) -> Result<(), AwsServiceError> {
    if change.asns.is_empty() {
        return Err(invalid_parameter_value("Asns must not be empty"));
    }
    let Some(m) = change.max_length else {
        return Ok(());
    };
    if !(0..=48).contains(&m) {
        return Err(invalid_parameter_value(
            "MaxLength must be between 0 and 48",
        ));
    }
    if cidr_prefix_len(&change.cidr).is_some_and(|prefix_len| m < prefix_len) {
        return Err(invalid_parameter_value(format!(
            "MaxLength must be greater than or equal to the prefix length of {}",
            change.cidr
        )));
    }
    Ok(())
}

/// Whether the write is legal against what the association already holds.
/// Kept separate from applying it so a DryRun reaches the same verdict as the
/// real call.
fn check_write(
    a: &IpamInternetRegistryAssociation,
    cidr: &str,
    write: RegistrationWrite,
) -> Result<(), AwsServiceError> {
    let registered = a.registrations.contains_key(cidr);
    match write {
        RegistrationWrite::Create if registered => Err(invalid_parameter_value(format!(
            "A routing policy registration already exists for {cidr}"
        ))),
        RegistrationWrite::Modify if !registered => Err(not_found(
            "InvalidIpamRoutingPolicyRegistration.NotFound",
            cidr,
        )),
        _ => Ok(()),
    }
}

/// Write one change into the association. A change that lands on a CIDR the
/// association already carries is a partial update: the model requires only
/// `Asns`, so a member the request leaves out keeps the value the registration
/// already carries instead of being silently cleared.
fn apply_write(
    a: &mut IpamInternetRegistryAssociation,
    change: &RegistrationChange,
    delta_id: &str,
) {
    let previous = a.registrations.get(&change.cidr).cloned();
    let creating = previous.is_none();
    a.registrations.insert(
        change.cidr.clone(),
        IpamRoutingPolicyRegistration {
            cidr: change.cidr.clone(),
            asns: change.asns.clone(),
            permit_more_specific_announcements: change.permit_more_specific_announcements.or_else(
                || {
                    previous
                        .as_ref()
                        .and_then(|p| p.permit_more_specific_announcements)
                },
            ),
            max_length: change
                .max_length
                .or_else(|| previous.as_ref().and_then(|p| p.max_length)),
            description: change
                .description
                .clone()
                .or_else(|| previous.as_ref().and_then(|p| p.description.clone())),
            latest_delta_id: delta_id.to_string(),
            state: if creating {
                "create-complete".to_string()
            } else {
                "update-complete".to_string()
            },
        },
    );
}

fn change_from_request(req: &AwsRequest) -> Result<RegistrationChange, AwsServiceError> {
    let cidr = require(&req.query_params, "Cidr")?;
    let max_length =
        match req.query_params.get("MaxLength").filter(|v| !v.is_empty()) {
            Some(v) => Some(v.parse::<i64>().map_err(|_| {
                invalid_parameter_value(format!("Invalid value '{v}' for MaxLength"))
            })?),
            None => None,
        };
    Ok(RegistrationChange {
        cidr,
        asns: indexed_list(&req.query_params, "Asn"),
        permit_more_specific_announcements: req
            .query_params
            .get("PermitMoreSpecificAnnouncements")
            .filter(|v| !v.is_empty())
            .map(|v| v.eq_ignore_ascii_case("true")),
        max_length,
        description: req
            .query_params
            .get("Description")
            .filter(|v| !v.is_empty())
            .cloned(),
    })
}

/// Shared body for Create and Modify: both take the same registration fields
/// and report the delta the change produced.
fn upsert_registration(
    svc: &Ec2Service,
    req: &AwsRequest,
    action: &'static str,
) -> Result<AwsResponse, AwsServiceError> {
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    let change = change_from_request(req)?;
    validate_change(&change)?;
    let token = client_token(req);
    let write = if action == "CreateIpamRoutingPolicyRegistration" {
        RegistrationWrite::Create
    } else {
        RegistrationWrite::Modify
    };

    let mut accounts = svc.state.write();
    let state = accounts.get_or_create(&req.account_id);
    let a = get_association(state, &id)?;
    require_enabled(a)?;
    // A retry replays the delta the first call produced instead of tripping
    // over the registration that call already wrote. A DryRun carrying a
    // served token replays too: it still changes nothing.
    if let Some(delta) = replay_delta(a, action, token.as_deref()) {
        return Ok(delta_response(action, req, &delta));
    }
    check_write(a, &change.cidr, write)?;
    // A DryRun validates the request -- including that the association exists,
    // that it is enabled, and that the CIDR is in the state this operation
    // needs -- and changes nothing, matching how the rest of EC2 treats one.
    if dry_run(req) {
        return Ok(Ec2Service::respond(action, &req.request_id, ""));
    }

    let delta_json = serde_json::json!({
        "action": if write == RegistrationWrite::Create { "create" } else { "modify" },
        "cidr": change.cidr,
        "asns": change.asns,
        "maxLength": change.max_length,
    })
    .to_string();
    let delta_id = push_delta(a, delta_json);
    record_client_token(a, action, token.as_deref(), &delta_id);
    apply_write(a, &change, &delta_id);
    let delta = a.deltas.last().expect("the delta was just pushed").clone();
    Ok(delta_response(action, req, &delta))
}

pub(crate) fn create_ipam_routing_policy_registration(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    upsert_registration(svc, req, "CreateIpamRoutingPolicyRegistration")
}

pub(crate) fn modify_ipam_routing_policy_registration(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    upsert_registration(svc, req, "ModifyIpamRoutingPolicyRegistration")
}

pub(crate) fn delete_ipam_routing_policy_registration(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    const ACTION: &str = "DeleteIpamRoutingPolicyRegistration";

    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    let cidr = require(&req.query_params, "Cidr")?;
    let token = client_token(req);
    let mut accounts = svc.state.write();
    let state = accounts.get_or_create(&req.account_id);
    let a = get_association(state, &id)?;
    if let Some(delta) = replay_delta(a, ACTION, token.as_deref()) {
        return Ok(delta_response(ACTION, req, &delta));
    }
    // A DryRun validates the request -- including that the association exists
    // and that the CIDR is registered -- and changes nothing, matching how the
    // rest of EC2 treats one.
    check_write(a, &cidr, RegistrationWrite::Modify)?;
    if dry_run(req) {
        return Ok(Ec2Service::respond(ACTION, &req.request_id, ""));
    }
    a.registrations.remove(&cidr);
    let delta_json = serde_json::json!({ "action": "delete", "cidr": cidr }).to_string();
    let delta_id = push_delta(a, delta_json);
    record_client_token(a, ACTION, token.as_deref(), &delta_id);
    let delta = a.deltas.last().expect("the delta was just pushed").clone();
    Ok(delta_response(ACTION, req, &delta))
}

/// One `add` entry of a batch document. Every field is typed, and a field the
/// caller spelled as the wrong JSON type fails the request rather than being
/// dropped: the delta records the document as published, so an entry that is
/// quietly skipped leaves an audit trail claiming a registration nobody made.
fn batch_addition(entry: &serde_json::Value) -> Result<RegistrationChange, AwsServiceError> {
    let cidr = entry
        .get("cidr")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            invalid_parameter_value("Each DeltaJson 'add' entry must carry a string 'cidr'")
        })?
        .to_string();
    let change = RegistrationChange {
        cidr,
        asns: batch_asns(entry)?,
        permit_more_specific_announcements: batch_field(
            entry,
            "permitMoreSpecificAnnouncements",
            serde_json::Value::as_bool,
            "a boolean",
        )?,
        max_length: batch_field(entry, "maxLength", serde_json::Value::as_i64, "an integer")?,
        description: batch_field(
            entry,
            "description",
            |v| v.as_str().map(str::to_string),
            "a string",
        )?,
    };
    validate_change(&change)?;
    Ok(change)
}

/// Read one optional batch-entry field, rejecting a value of the wrong type.
fn batch_field<T>(
    entry: &serde_json::Value,
    name: &str,
    read: impl Fn(&serde_json::Value) -> Option<T>,
    expected: &str,
) -> Result<Option<T>, AwsServiceError> {
    match entry.get(name) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => read(v).map(Some).ok_or_else(|| {
            invalid_parameter_value(format!("DeltaJson '{name}' must be {expected}"))
        }),
    }
}

/// `AsnList` is a list of strings on the wire, but an ASN is a number and a
/// hand-written delta document spells it as one. Both spellings are accepted;
/// anything else fails rather than yielding a registration that publishes no
/// route origin authorization at all.
fn batch_asns(entry: &serde_json::Value) -> Result<Vec<String>, AwsServiceError> {
    let asns = entry
        .get("asns")
        .ok_or_else(|| invalid_parameter_value("Each DeltaJson 'add' entry must carry 'asns'"))?
        .as_array()
        .ok_or_else(|| invalid_parameter_value("DeltaJson 'asns' must be an array"))?;
    asns.iter()
        .map(|v| match v {
            serde_json::Value::String(s) => Ok(s.clone()),
            serde_json::Value::Number(n) => Ok(n.to_string()),
            _ => Err(invalid_parameter_value(
                "DeltaJson 'asns' entries must be ASNs written as strings or numbers",
            )),
        })
        .collect()
}

/// The CIDRs a batch document removes: either bare strings or objects carrying
/// a `cidr`.
fn batch_removals(doc: &serde_json::Value) -> Result<Vec<String>, AwsServiceError> {
    let Some(entries) = doc.get("remove") else {
        return Ok(Vec::new());
    };
    entries
        .as_array()
        .ok_or_else(|| invalid_parameter_value("DeltaJson 'remove' must be an array"))?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .or_else(|| entry.get("cidr")?.as_str())
                .map(str::to_string)
                .ok_or_else(|| {
                    invalid_parameter_value(
                        "Each DeltaJson 'remove' entry must be a CIDR string or carry a string \
                         'cidr'",
                    )
                })
        })
        .collect()
}

/// A batch of registration changes, described by a JSON document rather than
/// indexed parameters. The whole batch lands as one delta.
pub(crate) fn batch_modify_ipam_routing_policy_registrations(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    const ACTION: &str = "BatchModifyIpamRoutingPolicyRegistrations";
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    let delta_json = require(&req.query_params, "DeltaJson")?;
    let parsed: serde_json::Value = serde_json::from_str(&delta_json)
        .map_err(|_| invalid_parameter_value("DeltaJson is not valid JSON"))?;
    let token = client_token(req);

    // The document lists the registrations to add and the CIDRs to remove.
    // Every entry is parsed and validated before anything is written: the
    // delta records the whole document as published, so one mistyped entry has
    // to fail the request rather than leave an audit trail claiming changes
    // that were never applied.
    let additions: Vec<RegistrationChange> = match parsed.get("add") {
        Some(add) => add
            .as_array()
            .ok_or_else(|| invalid_parameter_value("DeltaJson 'add' must be an array"))?
            .iter()
            .map(batch_addition)
            .collect::<Result<_, _>>()?,
        None => Vec::new(),
    };
    let removals = batch_removals(&parsed)?;

    let mut accounts = svc.state.write();
    let state = accounts.get_or_create(&req.account_id);
    let a = get_association(state, &id)?;
    require_enabled(a)?;
    if let Some(delta) = replay_delta(a, ACTION, token.as_deref()) {
        return Ok(delta_response(ACTION, req, &delta));
    }
    // Every entry goes through the same check the single operations use: an
    // `add` entry may create or update, but a `remove` entry for a CIDR that
    // was never registered removes nothing and must not be reported as
    // published.
    for change in &additions {
        check_write(a, &change.cidr, RegistrationWrite::Upsert)?;
    }
    for cidr in &removals {
        check_write(a, cidr, RegistrationWrite::Modify)?;
    }
    // A DryRun validates the request -- including every entry of the document
    // -- and changes nothing, matching how the rest of EC2 treats one.
    if dry_run(req) {
        return Ok(Ec2Service::respond(ACTION, &req.request_id, ""));
    }
    let delta_id = push_delta(a, delta_json.clone());
    record_client_token(a, ACTION, token.as_deref(), &delta_id);
    for change in &additions {
        apply_write(a, change, &delta_id);
    }
    for cidr in &removals {
        a.registrations.remove(cidr);
    }

    let delta = a.deltas.last().expect("the delta was just pushed").clone();
    Ok(delta_response(ACTION, req, &delta))
}

pub(crate) fn get_ipam_routing_policy_registrations(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let page = pagination(req)?;
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    let cidr = req.query_params.get("Cidr").filter(|v| !v.is_empty());
    let accounts = svc.state.read();
    let items: Vec<String> = accounts
        .get(&req.account_id)
        .and_then(|s| s.ipam_ir_associations.get(&id))
        .ok_or_else(|| association_not_found(&id))?
        .registrations
        .values()
        .filter(|r| cidr.is_none_or(|c| &r.cidr == c))
        .map(registration_xml)
        .collect();
    Ok(paged_response(
        "GetIpamRoutingPolicyRegistrations",
        req,
        "ipamRoutingPolicyRegistrationSet",
        &items,
        page,
    ))
}

pub(crate) fn get_ipam_routing_policy_registration_deltas(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let page = pagination(req)?;
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    validate_enum(
        &req.query_params,
        "ChronologicalOrder",
        &["forward", "reverse"],
    )?;
    let delta_id = req.query_params.get("DeltaId").filter(|v| !v.is_empty());
    let start = parse_time_bound(req, "StartTime")?;
    let end = parse_time_bound(req, "EndTime")?;

    let accounts = svc.state.read();
    let a = accounts
        .get(&req.account_id)
        .and_then(|s| s.ipam_ir_associations.get(&id))
        .ok_or_else(|| association_not_found(&id))?;

    let mut deltas: Vec<&IpamRoutingPolicyRegistrationDelta> = a
        .deltas
        .iter()
        .filter(|d| delta_id.is_none_or(|want| &d.delta_id == want))
        // Compare instants, not strings: the stored timestamps carry
        // milliseconds and an SDK omits them when they are zero, so a byte-wise
        // `>=` drops every delta in the same second as the bound.
        .filter(|d| start.is_none_or(|s| delta_time(d).is_none_or(|t| t >= s)))
        .filter(|d| end.is_none_or(|e| delta_time(d).is_none_or(|t| t <= e)))
        .collect();
    // Deltas are stored oldest first; `reverse` reports newest first.
    if req
        .query_params
        .get("ChronologicalOrder")
        .map(String::as_str)
        == Some("reverse")
    {
        deltas.reverse();
    }
    let items: Vec<String> = deltas.into_iter().map(delta_xml).collect();
    Ok(paged_response(
        "GetIpamRoutingPolicyRegistrationDeltas",
        req,
        "ipamRoutingPolicyRegistrationDeltaSet",
        &items,
        page,
    ))
}

/// The route origin authorizations an association publishes: one per
/// registration and ASN pair, which is the shape a relying party consumes.
pub(crate) fn get_ipam_route_origin_authorizations(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let page = pagination(req)?;
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    let cidr = req.query_params.get("Cidr").filter(|v| !v.is_empty());
    let accounts = svc.state.read();
    let a = accounts
        .get(&req.account_id)
        .and_then(|s| s.ipam_ir_associations.get(&id))
        .ok_or_else(|| association_not_found(&id))?;

    let mut items = Vec::new();
    for r in a.registrations.values() {
        if cidr.is_some_and(|c| &r.cidr != c) {
            continue;
        }
        for asn in &r.asns {
            let mut s = ec2_elem("cidr", &r.cidr) + &ec2_elem("asn", asn);
            if let Some(m) = r.max_length {
                s.push_str(&format!("<maxLength>{m}</maxLength>"));
            }
            items.push(s);
        }
    }
    Ok(paged_response(
        "GetIpamRouteOriginAuthorizations",
        req,
        "ipamRouteOriginAuthorizationSet",
        &items,
        page,
    ))
}

/// Per-ASN and per-CIDR views of what the registry has observed for an
/// association. Both derive from the registrations it publishes.
pub(crate) fn get_ipam_internet_registry_association_asns(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let page = pagination(req)?;
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    let filters = parse_filters(&req.query_params);
    let accounts = svc.state.read();
    let a = accounts
        .get(&req.account_id)
        .and_then(|s| s.ipam_ir_associations.get(&id))
        .ok_or_else(|| association_not_found(&id))?;

    let mut asns: Vec<&String> = a.registrations.values().flat_map(|r| &r.asns).collect();
    asns.sort();
    asns.dedup();
    let now = now_rfc3339();
    let items: Vec<String> = asns
        .into_iter()
        .filter(|asn| {
            matches_filters(&filters, |name| match name {
                "asn" => Some(vec![asn.to_string()]),
                _ => None,
            })
        })
        .map(|asn| ec2_elem("asn", asn) + &ec2_elem("lastObservedAt", &now))
        .collect();
    Ok(paged_response(
        "GetIpamInternetRegistryAssociationAsns",
        req,
        "ipamInternetRegistryAssociationAsnSet",
        &items,
        page,
    ))
}

pub(crate) fn get_ipam_internet_registry_association_cidrs(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let page = pagination(req)?;
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    let filters = parse_filters(&req.query_params);
    let accounts = svc.state.read();
    let a = accounts
        .get(&req.account_id)
        .and_then(|s| s.ipam_ir_associations.get(&id))
        .ok_or_else(|| association_not_found(&id))?;

    let now = now_rfc3339();
    let items: Vec<String> = a
        .registrations
        .keys()
        .filter(|cidr| {
            matches_filters(&filters, |name| match name {
                "cidr" => Some(vec![cidr.to_string()]),
                _ => None,
            })
        })
        .map(|cidr| ec2_elem("cidr", cidr) + &ec2_elem("lastObservedAt", &now))
        .collect();
    Ok(paged_response(
        "GetIpamInternetRegistryAssociationCidrs",
        req,
        "ipamInternetRegistryAssociationCidrSet",
        &items,
        page,
    ))
}

/// Routes a resource discovery has seen in a region. fakecloud runs no BGP
/// collector, so the discovered set is what the account's own registrations
/// advertise there rather than a fabricated view of the internet.
pub(crate) fn get_ipam_discovered_routes(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let page = pagination(req)?;
    let discovery_id = require(&req.query_params, "IpamResourceDiscoveryId")?;
    let resource_region = require(&req.query_params, "ResourceRegion")?;
    let filters = parse_filters(&req.query_params);
    let owner = req.account_id.clone();
    let accounts = svc.state.read();
    let state = accounts
        .get(&req.account_id)
        .ok_or_else(|| not_found("InvalidIpamResourceDiscoveryId.NotFound", &discovery_id))?;
    if !state.ipam_resource_discoveries.contains_key(&discovery_id) {
        return Err(not_found(
            "InvalidIpamResourceDiscoveryId.NotFound",
            &discovery_id,
        ));
    }

    let now = now_rfc3339();
    let mut items = Vec::new();
    for a in state.ipam_ir_associations.values() {
        if a.region != resource_region {
            continue;
        }
        for r in a.registrations.values() {
            let asn = r.asns.first().cloned().unwrap_or_default();
            let keep = matches_filters(&filters, |name| match name {
                "ipam-resource-discovery-id" => Some(vec![discovery_id.clone()]),
                "resource-region" => Some(vec![resource_region.clone()]),
                "resource-owner-id" => Some(vec![owner.clone()]),
                "cidr" => Some(vec![r.cidr.clone()]),
                "asn" => Some(r.asns.clone()),
                "state" => Some(vec!["advertised".to_string()]),
                _ => None,
            });
            if !keep {
                continue;
            }
            items.push(format!(
                "{}{}{}{}{}{}{}",
                ec2_elem("ipamResourceDiscoveryId", &discovery_id),
                ec2_elem("resourceRegion", &resource_region),
                ec2_elem("resourceOwnerId", &owner),
                ec2_elem("cidr", &r.cidr),
                ec2_elem("asn", &asn),
                ec2_elem("state", "advertised"),
                ec2_elem("sampleTime", &now),
            ));
        }
    }
    Ok(paged_response(
        "GetIpamDiscoveredRoutes",
        req,
        "ipamDiscoveredRouteSet",
        &items,
        page,
    ))
}

/// Route protection findings: a registration whose CIDR is authorized for its
/// ASNs is `valid`; one an association publishes with no ASN at all is
/// `unknown`, which is what an unsigned announcement looks like to RPKI.
pub(crate) fn get_ipam_route_protection_findings(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let (max_results, next_token) = pagination(req)?;
    let ipam_id = require(&req.query_params, "IpamId")?;
    let filters = parse_filters(&req.query_params);
    let owner = req.account_id.clone();
    let accounts = svc.state.read();
    let state = accounts
        .get(&req.account_id)
        .ok_or_else(|| not_found("InvalidIpamId.NotFound", &ipam_id))?;
    if !state.ipams.contains_key(&ipam_id) {
        return Err(not_found("InvalidIpamId.NotFound", &ipam_id));
    }

    let now = now_rfc3339();
    let mut items = Vec::new();
    for a in state.ipam_ir_associations.values() {
        if a.ipam_id != ipam_id {
            continue;
        }
        for r in a.registrations.values() {
            let asn = r.asns.first().cloned().unwrap_or_default();
            // `IpamRpkiStrength` is `strict | permissive`. A registration that
            // names its origin ASNs authorizes exactly those, which is the
            // strict posture; one with none authorizes nothing specific.
            let (status, strength) = if r.asns.is_empty() {
                ("unknown", "permissive")
            } else {
                ("valid", "strict")
            };
            let keep = matches_filters(&filters, |name| match name {
                "resource-owner-id" => Some(vec![owner.clone()]),
                "resource-region" => Some(vec![a.region.clone()]),
                "cidr" => Some(vec![r.cidr.clone()]),
                "asn" => Some(r.asns.clone()),
                "rpki-status" => Some(vec![status.to_string()]),
                "rpki-strength" => Some(vec![strength.to_string()]),
                _ => None,
            });
            if !keep {
                continue;
            }
            // A finding's `roaSet` holds `IpamRouteOriginAuthorization`, whose
            // prefix member is `prefix`. The `cidr` spelling belongs to
            // `IpamRouteOriginAuthorizationInfo`, the shape
            // GetIpamRouteOriginAuthorizations returns -- emitting it here
            // makes an SDK read the prefix as absent.
            let roas: Vec<String> = r
                .asns
                .iter()
                .map(|asn| {
                    let mut s = ec2_elem("asn", asn) + &ec2_elem("prefix", &r.cidr);
                    if let Some(m) = r.max_length {
                        s.push_str(&format!("<maxLength>{m}</maxLength>"));
                    }
                    s
                })
                .collect();
            let mut finding = format!(
                "{}{}{}{}{}{}{}",
                ec2_elem("resourceOwnerId", &owner),
                ec2_elem("resourceRegion", &a.region),
                ec2_elem("cidr", &r.cidr),
                ec2_elem("asn", &asn),
                ec2_elem("rpkiStatus", status),
                ec2_elem("rpkiStrength", strength),
                ec2_elem("sampleTime", &now),
            );
            if !roas.is_empty() {
                finding.push_str(&ec2_list("roaSet", &roas));
            }
            items.push(finding);
        }
    }
    let (page, token) = paginate(&items, next_token.as_deref(), max_results);
    Ok(Ec2Service::respond(
        "GetIpamRouteProtectionFindings",
        &req.request_id,
        &format!(
            "{}{}{}",
            ec2_elem("ipamId", &ipam_id),
            ec2_list("routeProtectionFindingSet", &page),
            token.map(|t| ec2_elem("nextToken", &t)).unwrap_or_default()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ec2_request as req, err_of};

    fn body(resp: AwsResponse) -> String {
        String::from_utf8_lossy(resp.body.expect_bytes()).to_string()
    }

    /// Register an IPAM directly so an association has something to attach to.
    fn seed_ipam(svc: &Ec2Service) {
        let mut accounts = svc.state.write();
        let state = accounts.get_or_create("000000000000");
        state.ipams.insert(
            "ipam-1".to_string(),
            crate::state::Ipam {
                id: "ipam-1".to_string(),
                public_scope_id: "ipam-scope-pub".to_string(),
                private_scope_id: "ipam-scope-priv".to_string(),
                tier: "advanced".to_string(),
                description: String::new(),
            },
        );
    }

    /// Create an association, without enabling it: it cannot publish yet.
    fn make_pending_association(svc: &Ec2Service) -> String {
        seed_ipam(svc);
        let b = body(
            create_ipam_internet_registry_association(
                svc,
                &req(
                    "CreateIpamInternetRegistryAssociation",
                    &[
                        ("IpamId", "ipam-1"),
                        ("Rir", "arin"),
                        ("OrganizationHandle", "ORG-1"),
                    ],
                ),
            )
            .unwrap(),
        );
        b.split("<ipamInternetRegistryAssociationId>")
            .nth(1)
            .unwrap()
            .split("</ipamInternetRegistryAssociationId>")
            .next()
            .unwrap()
            .to_string()
    }

    fn enable(svc: &Ec2Service, id: &str, service_uri: &str, child_handle: &str) -> String {
        body(
            enable_ipam_internet_registry_association(
                svc,
                &req(
                    "EnableIpamInternetRegistryAssociation",
                    &[
                        ("IpamInternetRegistryAssociationId", id),
                        ("RpkiVersion", "1"),
                        ("ServiceUri", service_uri),
                        ("ChildHandle", child_handle),
                        ("ParentHandle", "parent"),
                        ("ParentBpkiTa", "TA=="),
                    ],
                ),
            )
            .unwrap(),
        )
    }

    /// An association that has been enabled against the registry, which is
    /// what a registration needs.
    fn make_association(svc: &Ec2Service) -> String {
        let id = make_pending_association(svc);
        enable(svc, &id, "https://rpki.example/up-down", "child");
        id
    }

    fn register(svc: &Ec2Service, id: &str, cidr: &str, max_length: Option<&str>) {
        let mut params: Vec<(&str, &str)> = vec![
            ("IpamInternetRegistryAssociationId", id),
            ("Cidr", cidr),
            ("Asn.1", "64512"),
        ];
        if let Some(m) = max_length {
            params.push(("MaxLength", m));
        }
        create_ipam_routing_policy_registration(
            svc,
            &req("CreateIpamRoutingPolicyRegistration", &params),
        )
        .unwrap();
    }

    fn registrations(svc: &Ec2Service, id: &str, params: &[(&str, &str)]) -> String {
        let mut all: Vec<(&str, &str)> = vec![("IpamInternetRegistryAssociationId", id)];
        all.extend_from_slice(params);
        body(
            get_ipam_routing_policy_registrations(
                svc,
                &req("GetIpamRoutingPolicyRegistrations", &all),
            )
            .unwrap(),
        )
    }

    fn stored_child_request(svc: &Ec2Service, id: &str) -> String {
        svc.state
            .read()
            .get("000000000000")
            .unwrap()
            .ipam_ir_associations
            .get(id)
            .unwrap()
            .child_request_xml
            .clone()
            .unwrap()
    }

    fn batch(svc: &Ec2Service, id: &str, delta_json: &str) -> Result<AwsResponse, AwsServiceError> {
        batch_modify_ipam_routing_policy_registrations(
            svc,
            &req(
                "BatchModifyIpamRoutingPolicyRegistrations",
                &[
                    ("IpamInternetRegistryAssociationId", id),
                    ("DeltaJson", delta_json),
                ],
            ),
        )
    }

    /// A finding's `roaSet` carries `IpamRouteOriginAuthorization`, whose
    /// prefix member is `prefix`; `cidr` belongs to a different shape and an
    /// SDK discards it.
    #[test]
    fn route_protection_findings_use_the_modeled_roa_members() {
        let svc = Ec2Service::new();
        let id = make_association(&svc);
        register(&svc, &id, "192.0.2.0/24", Some("24"));

        let b = body(
            get_ipam_route_protection_findings(
                &svc,
                &req("GetIpamRouteProtectionFindings", &[("IpamId", "ipam-1")]),
            )
            .unwrap(),
        );
        assert!(b.contains("<prefix>192.0.2.0/24</prefix>"), "{b}");
        assert!(
            !b.contains("<roaSet><item><cidr>"),
            "cidr is the wrong member name here: {b}"
        );
        // `IpamRpkiStrength` is `strict | permissive` -- nothing else.
        assert!(b.contains("<rpkiStrength>strict</rpkiStrength>"), "{b}");
        assert!(!b.contains("strong"), "{b}");
    }

    /// The child request is a document the caller hands to the RIR, so every
    /// value interpolated into it has to be entity-escaped. The response
    /// escapes the blob as a whole, so an unescaped `&` or `"` would look fine
    /// on the wire and only break when the registry parses the document.
    #[test]
    fn the_child_request_escapes_every_interpolated_value() {
        let svc = Ec2Service::new();
        let id = make_pending_association(&svc);
        enable(
            &svc,
            &id,
            "https://rpki.example/up-down?src=a&v=2",
            "ch\"ild",
        );

        let doc = stored_child_request(&svc, &id);
        assert!(
            doc.contains("service_uri=\"https://rpki.example/up-down?src=a&amp;v=2\""),
            "{doc}"
        );
        assert!(doc.contains("child_handle=\"ch&quot;ild\""), "{doc}");
        // The attribute never closes early, so the document stays parseable.
        assert_eq!(doc.matches('"').count(), 8, "{doc}");
    }

    /// A DryRun validates the request, so it cannot report success for an
    /// association that does not exist.
    #[test]
    fn a_dry_run_still_resolves_the_association() {
        let svc = Ec2Service::new();
        let missing = "ipam-ir-assoc-nope";
        for r in [
            delete_ipam_internet_registry_association(
                &svc,
                &req(
                    "DeleteIpamInternetRegistryAssociation",
                    &[
                        ("IpamInternetRegistryAssociationId", missing),
                        ("DryRun", "true"),
                    ],
                ),
            ),
            delete_ipam_routing_policy_registration(
                &svc,
                &req(
                    "DeleteIpamRoutingPolicyRegistration",
                    &[
                        ("IpamInternetRegistryAssociationId", missing),
                        ("Cidr", "192.0.2.0/24"),
                        ("DryRun", "true"),
                    ],
                ),
            ),
        ] {
            assert_eq!(
                err_of(r).code(),
                "InvalidIpamInternetRegistryAssociationId.NotFound"
            );
        }

        // And a dry run against a live association changes nothing.
        let id = make_association(&svc);
        register(&svc, &id, "192.0.2.0/24", None);
        delete_ipam_routing_policy_registration(
            &svc,
            &req(
                "DeleteIpamRoutingPolicyRegistration",
                &[
                    ("IpamInternetRegistryAssociationId", &id),
                    ("Cidr", "192.0.2.0/24"),
                    ("DryRun", "true"),
                ],
            ),
        )
        .unwrap();
        assert!(registrations(&svc, &id, &[]).contains("192.0.2.0/24"));
    }

    /// A dry run reaches the same verdict as the real call, so it runs after
    /// every existence check rather than before them: otherwise it reports
    /// success and the call it was rehearsing fails.
    #[test]
    fn a_dry_run_reaches_the_same_verdict_as_the_real_call() {
        let svc = Ec2Service::new();
        let id = make_association(&svc);
        register(&svc, &id, "192.0.2.0/24", None);

        // Creating an already-registered CIDR is a conflict, dry run or not.
        let err = err_of(create_ipam_routing_policy_registration(
            &svc,
            &req(
                "CreateIpamRoutingPolicyRegistration",
                &[
                    ("IpamInternetRegistryAssociationId", &id),
                    ("Cidr", "192.0.2.0/24"),
                    ("Asn.1", "64512"),
                    ("DryRun", "true"),
                ],
            ),
        ));
        assert_eq!(err.code(), "InvalidParameterValue");

        // And modifying one that was never registered is still a not-found.
        let err = err_of(modify_ipam_routing_policy_registration(
            &svc,
            &req(
                "ModifyIpamRoutingPolicyRegistration",
                &[
                    ("IpamInternetRegistryAssociationId", &id),
                    ("Cidr", "198.51.100.0/24"),
                    ("Asn.1", "64512"),
                    ("DryRun", "true"),
                ],
            ),
        ));
        assert_eq!(err.code(), "InvalidIpamRoutingPolicyRegistration.NotFound");

        // A dry-run create naming an IPAM that does not exist is the very
        // failure a dry run exists to surface.
        let err = err_of(create_ipam_internet_registry_association(
            &svc,
            &req(
                "CreateIpamInternetRegistryAssociation",
                &[
                    ("IpamId", "ipam-ghost"),
                    ("Rir", "arin"),
                    ("OrganizationHandle", "ORG-1"),
                    ("DryRun", "true"),
                ],
            ),
        ));
        assert_eq!(err.code(), "InvalidIpamId.NotFound");
    }

    /// The model requires the registrations to be removed before the
    /// association goes, so a delete that would orphan published ROAs is
    /// refused -- on a dry run exactly as for real.
    #[test]
    fn deleting_an_association_requires_its_registrations_to_be_gone() {
        let svc = Ec2Service::new();
        let id = make_association(&svc);
        register(&svc, &id, "192.0.2.0/24", None);

        for dry in ["false", "true"] {
            let err = err_of(delete_ipam_internet_registry_association(
                &svc,
                &req(
                    "DeleteIpamInternetRegistryAssociation",
                    &[("IpamInternetRegistryAssociationId", &id), ("DryRun", dry)],
                ),
            ));
            assert_eq!(err.code(), "DependencyViolation", "DryRun={dry}");
        }
        // The association and its registration are both still there.
        assert!(registrations(&svc, &id, &[]).contains("192.0.2.0/24"));

        delete_ipam_routing_policy_registration(
            &svc,
            &req(
                "DeleteIpamRoutingPolicyRegistration",
                &[
                    ("IpamInternetRegistryAssociationId", &id),
                    ("Cidr", "192.0.2.0/24"),
                ],
            ),
        )
        .unwrap();
        let b = body(
            delete_ipam_internet_registry_association(
                &svc,
                &req(
                    "DeleteIpamInternetRegistryAssociation",
                    &[("IpamInternetRegistryAssociationId", &id)],
                ),
            )
            .unwrap(),
        );
        assert!(b.contains("<state>delete-complete</state>"), "{b}");
    }

    /// `MaxLength` carries `@range 0..48` and must cover at least the prefix.
    #[test]
    fn max_length_is_bounded_by_the_model_and_the_prefix() {
        let svc = Ec2Service::new();
        let id = make_association(&svc);
        for bad in ["49", "200", "16"] {
            let err = err_of(create_ipam_routing_policy_registration(
                &svc,
                &req(
                    "CreateIpamRoutingPolicyRegistration",
                    &[
                        ("IpamInternetRegistryAssociationId", &id),
                        ("Cidr", "192.0.2.0/24"),
                        ("Asn.1", "64512"),
                        ("MaxLength", bad),
                    ],
                ),
            ));
            assert_eq!(err.code(), "InvalidParameterValue", "MaxLength={bad}");
        }
        register(&svc, &id, "192.0.2.0/24", Some("32"));
    }

    /// Modify takes only `Asns` as required, so the members it leaves out keep
    /// the values the registration already carries instead of being cleared.
    #[test]
    fn modify_is_a_partial_update() {
        let svc = Ec2Service::new();
        let id = make_association(&svc);
        create_ipam_routing_policy_registration(
            &svc,
            &req(
                "CreateIpamRoutingPolicyRegistration",
                &[
                    ("IpamInternetRegistryAssociationId", &id),
                    ("Cidr", "10.0.0.0/16"),
                    ("Asn.1", "64512"),
                    ("MaxLength", "24"),
                    ("Description", "prod prefix"),
                    ("PermitMoreSpecificAnnouncements", "true"),
                ],
            ),
        )
        .unwrap();

        modify_ipam_routing_policy_registration(
            &svc,
            &req(
                "ModifyIpamRoutingPolicyRegistration",
                &[
                    ("IpamInternetRegistryAssociationId", &id),
                    ("Cidr", "10.0.0.0/16"),
                    ("Asn.1", "64513"),
                ],
            ),
        )
        .unwrap();

        let b = registrations(&svc, &id, &[]);
        assert!(b.contains("<item>64513</item>"), "{b}");
        assert!(b.contains("<maxLength>24</maxLength>"), "{b}");
        assert!(b.contains("<description>prod prefix</description>"), "{b}");
        assert!(
            b.contains("<permitMoreSpecificAnnouncements>true</permitMoreSpecificAnnouncements>"),
            "{b}"
        );
        assert!(b.contains("<state>update-complete</state>"), "{b}");

        // The ROAs the registration publishes keep the max length too.
        let b = body(
            get_ipam_route_origin_authorizations(
                &svc,
                &req(
                    "GetIpamRouteOriginAuthorizations",
                    &[("IpamInternetRegistryAssociationId", &id)],
                ),
            )
            .unwrap(),
        );
        assert!(b.contains("<maxLength>24</maxLength>"), "{b}");
    }

    /// A registration publishes through the association's RPKI service, which
    /// only exists once the association has been enabled.
    #[test]
    fn a_registration_needs_an_enabled_association() {
        let svc = Ec2Service::new();
        let id = make_pending_association(&svc);

        let err = err_of(create_ipam_routing_policy_registration(
            &svc,
            &req(
                "CreateIpamRoutingPolicyRegistration",
                &[
                    ("IpamInternetRegistryAssociationId", &id),
                    ("Cidr", "192.0.2.0/24"),
                    ("Asn.1", "64512"),
                ],
            ),
        ));
        assert_eq!(err.code(), "IncorrectState");

        let err = err_of(batch(
            &svc,
            &id,
            r#"{"add":[{"cidr":"192.0.2.0/24","asns":["64512"]}]}"#,
        ));
        assert_eq!(err.code(), "IncorrectState");

        // Enabling it opens the association up.
        enable(&svc, &id, "https://rpki.example/up-down", "child");
        register(&svc, &id, "192.0.2.0/24", None);
        assert!(registrations(&svc, &id, &[]).contains("192.0.2.0/24"));
    }

    /// An ASN is a number, and a hand-written delta document spells it as one.
    /// Dropping those ASNs would publish a registration authorizing nobody.
    #[test]
    fn a_batch_accepts_asns_written_as_numbers() {
        let svc = Ec2Service::new();
        let id = make_association(&svc);
        batch(
            &svc,
            &id,
            r#"{"add":[{"cidr":"192.0.2.0/24","asns":[64512,"64513"]}]}"#,
        )
        .unwrap();

        let b = registrations(&svc, &id, &[]);
        assert!(b.contains("<item>64512</item>"), "{b}");
        assert!(b.contains("<item>64513</item>"), "{b}");

        // And the registration publishes the route origin authorizations that
        // make the finding `valid` rather than `unknown`.
        let b = body(
            get_ipam_route_protection_findings(
                &svc,
                &req("GetIpamRouteProtectionFindings", &[("IpamId", "ipam-1")]),
            )
            .unwrap(),
        );
        assert!(b.contains("<rpkiStatus>valid</rpkiStatus>"), "{b}");
    }

    /// The delta records the whole document as published, so an entry the
    /// caller mistyped fails the request instead of leaving an audit trail
    /// claiming registrations that were never applied.
    #[test]
    fn a_batch_entry_that_cannot_be_applied_fails_the_whole_request() {
        let svc = Ec2Service::new();
        let id = make_association(&svc);
        for bad in [
            r#"{"add":[{"cidr":"192.0.2.0/24","asns":["64512"]},{"asns":["64513"]}]}"#,
            r#"{"add":[{"cidr":"192.0.2.0/24","asns":["64512"]},{"cidr":198,"asns":["64513"]}]}"#,
            r#"{"add":[{"cidr":"10.0.0.0/16","asns":["64512"],"maxLength":200}]}"#,
            r#"{"add":[{"cidr":"10.0.0.0/16","asns":["64512"],"maxLength":8}]}"#,
            r#"{"add":[{"cidr":"10.0.0.0/16","asns":[]}]}"#,
            r#"{"add":[{"cidr":"10.0.0.0/16"}]}"#,
            r#"{"add":[{"cidr":"10.0.0.0/16","asns":["64512"],"maxLength":"24"}]}"#,
            r#"{"remove":[{"asn":"64512"}]}"#,
        ] {
            let err = err_of(batch(&svc, &id, bad));
            assert_eq!(err.code(), "InvalidParameterValue", "{bad}");
        }
        // Removing a CIDR that was never registered is a not-found, not a
        // delta claiming a removal that did not happen.
        let err = err_of(batch(&svc, &id, r#"{"remove":["203.0.113.0/24"]}"#));
        assert_eq!(err.code(), "InvalidIpamRoutingPolicyRegistration.NotFound");

        // Nothing was applied and no delta was recorded.
        let b = registrations(&svc, &id, &[]);
        assert!(!b.contains("192.0.2.0/24"), "{b}");
        let b = body(
            get_ipam_routing_policy_registration_deltas(
                &svc,
                &req(
                    "GetIpamRoutingPolicyRegistrationDeltas",
                    &[("IpamInternetRegistryAssociationId", &id)],
                ),
            )
            .unwrap(),
        );
        assert!(!b.contains("<deltaId>"), "{b}");
    }

    /// A batch `add` is documented to create or update, so a second document
    /// for the same CIDR updates it -- partially, the way Modify does.
    #[test]
    fn a_batch_add_updates_a_cidr_it_already_registered() {
        let svc = Ec2Service::new();
        let id = make_association(&svc);
        batch(
            &svc,
            &id,
            r#"{"add":[{"cidr":"10.0.0.0/16","asns":["64512"],"maxLength":24}]}"#,
        )
        .unwrap();
        batch(
            &svc,
            &id,
            r#"{"add":[{"cidr":"10.0.0.0/16","asns":[64513]}]}"#,
        )
        .unwrap();

        let b = registrations(&svc, &id, &[]);
        assert_eq!(b.matches("<cidr>10.0.0.0/16</cidr>").count(), 1, "{b}");
        assert!(b.contains("<item>64513</item>"), "{b}");
        assert!(b.contains("<maxLength>24</maxLength>"), "{b}");
        assert!(b.contains("<state>update-complete</state>"), "{b}");
    }

    /// A retry under the original `ClientToken` replays the first call's
    /// result instead of failing on the change that call already made.
    #[test]
    fn a_client_token_replays_the_original_result() {
        let svc = Ec2Service::new();
        seed_ipam(&svc);
        let create = |token: &str| {
            body(
                create_ipam_internet_registry_association(
                    &svc,
                    &req(
                        "CreateIpamInternetRegistryAssociation",
                        &[
                            ("IpamId", "ipam-1"),
                            ("Rir", "arin"),
                            ("OrganizationHandle", "ORG-1"),
                            ("ClientToken", token),
                        ],
                    ),
                )
                .unwrap(),
            )
        };
        let first = create("token-a");
        assert_eq!(first, create("token-a"), "the retry replays the original");
        assert_ne!(first, create("token-b"), "a new token is a new association");

        let id = make_association(&svc);
        let register_once = |token: &str| {
            body(
                create_ipam_routing_policy_registration(
                    &svc,
                    &req(
                        "CreateIpamRoutingPolicyRegistration",
                        &[
                            ("IpamInternetRegistryAssociationId", &id),
                            ("Cidr", "192.0.2.0/24"),
                            ("Asn.1", "64512"),
                            ("ClientToken", token),
                        ],
                    ),
                )
                .unwrap(),
            )
        };
        let first = register_once("token-c");
        assert_eq!(first, register_once("token-c"));
        // The replay minted no second delta and no second registration.
        let b = registrations(&svc, &id, &[]);
        assert_eq!(b.matches("<cidr>192.0.2.0/24</cidr>").count(), 1, "{b}");
        let b = body(
            get_ipam_routing_policy_registration_deltas(
                &svc,
                &req(
                    "GetIpamRoutingPolicyRegistrationDeltas",
                    &[("IpamInternetRegistryAssociationId", &id)],
                ),
            )
            .unwrap(),
        );
        assert_eq!(b.matches("<deltaId>").count(), 1, "{b}");
    }

    /// A time bound is compared as an instant, so a delta recorded in the same
    /// second as the bound is not silently dropped, and a malformed bound is
    /// rejected rather than filtering everything out.
    #[test]
    fn delta_time_bounds_compare_instants() {
        let svc = Ec2Service::new();
        let id = make_association(&svc);
        register(&svc, &id, "192.0.2.0/24", None);

        // A whole-second bound at the epoch start still includes the delta.
        let b = body(
            get_ipam_routing_policy_registration_deltas(
                &svc,
                &req(
                    "GetIpamRoutingPolicyRegistrationDeltas",
                    &[
                        ("IpamInternetRegistryAssociationId", &id),
                        ("StartTime", "2000-01-01T00:00:00Z"),
                    ],
                ),
            )
            .unwrap(),
        );
        assert!(b.contains("<deltaId>"), "{b}");

        let err = err_of(get_ipam_routing_policy_registration_deltas(
            &svc,
            &req(
                "GetIpamRoutingPolicyRegistrationDeltas",
                &[
                    ("IpamInternetRegistryAssociationId", &id),
                    ("StartTime", "banana"),
                ],
            ),
        ));
        assert_eq!(err.code(), "InvalidParameterValue");
    }

    /// `MaxResults` bounds a page and the `nextToken` it returns fetches the
    /// rest, rather than every read handing back the whole set.
    #[test]
    fn reads_page_and_round_trip_the_next_token() {
        let svc = Ec2Service::new();
        let id = make_association(&svc);
        for i in 0..7 {
            register(&svc, &id, &format!("10.{i}.0.0/16"), None);
        }

        let first = registrations(&svc, &id, &[("MaxResults", "5")]);
        assert_eq!(first.matches("<cidr>").count(), 5, "{first}");
        let token = first
            .split("<nextToken>")
            .nth(1)
            .unwrap_or_else(|| panic!("no nextToken in {first}"))
            .split("</nextToken>")
            .next()
            .unwrap()
            .to_string();

        let second = registrations(&svc, &id, &[("MaxResults", "5"), ("NextToken", &token)]);
        assert_eq!(second.matches("<cidr>").count(), 2, "{second}");
        assert!(!second.contains("<nextToken>"), "{second}");

        // The deltas each registration produced page the same way.
        let b = body(
            get_ipam_routing_policy_registration_deltas(
                &svc,
                &req(
                    "GetIpamRoutingPolicyRegistrationDeltas",
                    &[
                        ("IpamInternetRegistryAssociationId", &id),
                        ("MaxResults", "5"),
                    ],
                ),
            )
            .unwrap(),
        );
        assert_eq!(b.matches("<deltaId>").count(), 5, "{b}");
        assert!(b.contains("<nextToken>"), "{b}");

        // `IpamMaxResults` is `@range 5..1000`.
        let err = err_of(get_ipam_routing_policy_registrations(
            &svc,
            &req(
                "GetIpamRoutingPolicyRegistrations",
                &[
                    ("IpamInternetRegistryAssociationId", &id),
                    ("MaxResults", "1"),
                ],
            ),
        ));
        assert_eq!(err.code(), "InvalidParameterValue");
    }

    /// The `Filters` these operations model narrow the result rather than
    /// being accepted and discarded.
    #[test]
    fn filters_narrow_the_results() {
        let svc = Ec2Service::new();
        let id = make_association(&svc);
        register(&svc, &id, "192.0.2.0/24", None);

        let describe = |params: &[(&str, &str)]| {
            body(
                describe_ipam_internet_registry_associations(
                    &svc,
                    &req("DescribeIpamInternetRegistryAssociations", params),
                )
                .unwrap(),
            )
        };
        assert!(describe(&[("Filter.1.Name", "rir"), ("Filter.1.Value.1", "arin")]).contains(&id));
        assert!(!describe(&[("Filter.1.Name", "rir"), ("Filter.1.Value.1", "ripe")]).contains(&id));
        assert!(
            !describe(&[("Filter.1.Name", "nonsense"), ("Filter.1.Value.1", "arin")]).contains(&id),
            "an unknown filter name matches nothing"
        );

        // The per-CIDR and per-ASN views filter too.
        let cidrs = body(
            get_ipam_internet_registry_association_cidrs(
                &svc,
                &req(
                    "GetIpamInternetRegistryAssociationCidrs",
                    &[
                        ("IpamInternetRegistryAssociationId", &id),
                        ("Filter.1.Name", "cidr"),
                        ("Filter.1.Value.1", "198.51.100.0/24"),
                    ],
                ),
            )
            .unwrap(),
        );
        assert!(!cidrs.contains("192.0.2.0/24"), "{cidrs}");
        let asns = body(
            get_ipam_internet_registry_association_asns(
                &svc,
                &req(
                    "GetIpamInternetRegistryAssociationAsns",
                    &[
                        ("IpamInternetRegistryAssociationId", &id),
                        ("Filter.1.Name", "asn"),
                        ("Filter.1.Value.1", "64512"),
                    ],
                ),
            )
            .unwrap(),
        );
        assert!(asns.contains("<asn>64512</asn>"), "{asns}");
    }
}
