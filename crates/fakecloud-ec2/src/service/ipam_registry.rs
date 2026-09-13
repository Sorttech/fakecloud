//! IPAM internet-registry associations and the routing policy registrations
//! (RPKI route origin authorizations) published through them.
//!
//! An association ties an IPAM to one Regional Internet Registry. Registrations
//! hang off it, keyed by CIDR, and every change to them produces a delta: the
//! deltas are the audit trail, so they outlive the registrations they describe.

use chrono::Utc;

use fakecloud_aws::ec2query::{ec2_elem, ec2_list};
use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::service::Ec2Service;
use crate::service_helpers::{
    gen_id, indexed_list, invalid_parameter_value, not_found, require, validate_enum,
    validate_max_results,
};
use crate::state::{
    Ec2State, IpamInternetRegistryAssociation, IpamRoutingPolicyRegistration,
    IpamRoutingPolicyRegistrationDelta, Tag,
};

const RIRS: &[&str] = &["ripe", "apnic", "arin", "lacnic"];

fn mr(req: &AwsRequest) -> Result<(), AwsServiceError> {
    validate_max_results(&req.query_params, 5, 1000)
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
    if dry_run(req) {
        return Ok(Ec2Service::respond(
            "CreateIpamInternetRegistryAssociation",
            &req.request_id,
            "",
        ));
    }

    let owner = req.account_id.clone();
    let region = region_of(req);
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
    };

    let mut accounts = svc.state.write();
    let state = accounts.get_or_create(&req.account_id);
    if !state.ipams.contains_key(&association.ipam_id) {
        return Err(not_found("InvalidIpamId.NotFound", &association.ipam_id));
    }
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
    // The child request is the RPKI provisioning document the registry needs;
    // it is what the caller takes to the RIR to finish setup.
    a.child_request_xml = Some(format!(
        "<publisher_request version=\"{rpki_version}\" \
         service_uri=\"{service_uri}\" \
         child_handle=\"{child_handle}\" \
         parent_handle=\"{parent_handle}\">\
         <parent_bpki_ta>{parent_bpki_ta}</parent_bpki_ta>\
         </publisher_request>"
    ));
    a.state = "enable-complete".to_string();
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
    if !state.ipam_ir_associations.contains_key(&id) {
        return Err(association_not_found(&id));
    }
    // A DryRun validates the request -- including that the association exists
    // -- and changes nothing, matching how the rest of EC2 treats one.
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
    // The response reports the association in its terminal state; the
    // registrations published through it go with it.
    association.state = "delete-complete".to_string();
    association.registrations.clear();
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
    mr(req)?;
    let ids = indexed_list(&req.query_params, "IpamInternetRegistryAssociationId");
    let owner = req.account_id.clone();
    let accounts = svc.state.read();
    let mut items = Vec::new();
    if let Some(state) = accounts.get(&req.account_id) {
        for (id, a) in &state.ipam_ir_associations {
            if !ids.is_empty() && !ids.contains(id) {
                continue;
            }
            let tags = state.tags.get(id).cloned().unwrap_or_default();
            items.push(association_xml(a, &owner, &tags));
        }
    }
    Ok(Ec2Service::respond(
        "DescribeIpamInternetRegistryAssociations",
        &req.request_id,
        &ec2_list("ipamInternetRegistryAssociationSet", &items),
    ))
}

// ---- routing policy registrations ----

/// Shared body for Create and Modify: both take the same registration fields
/// and report the delta the change produced.
fn upsert_registration(
    svc: &Ec2Service,
    req: &AwsRequest,
    action: &'static str,
) -> Result<AwsResponse, AwsServiceError> {
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    let cidr = require(&req.query_params, "Cidr")?;
    let asns = indexed_list(&req.query_params, "Asn");
    if asns.is_empty() {
        return Err(invalid_parameter_value("Asns must not be empty"));
    }
    let max_length =
        match req.query_params.get("MaxLength").filter(|v| !v.is_empty()) {
            Some(v) => Some(v.parse::<i64>().map_err(|_| {
                invalid_parameter_value(format!("Invalid value '{v}' for MaxLength"))
            })?),
            None => None,
        };
    // `IpamRoutingPolicyRegistrationMaxLength` carries `@range 0..48`, and the
    // member documents that it must not be shorter than the CIDR's own prefix
    // length -- a ROA that authorizes less than the prefix it covers announces
    // nothing.
    crate::service_helpers::validate_int_range(&req.query_params, "MaxLength", 0, 48)?;
    if let (Some(m), Some(prefix_len)) = (max_length, cidr_prefix_len(&cidr)) {
        if m < prefix_len {
            return Err(invalid_parameter_value(format!(
                "MaxLength must be greater than or equal to the prefix length of {cidr}"
            )));
        }
    }

    let mut accounts = svc.state.write();
    let state = accounts.get_or_create(&req.account_id);
    let a = get_association(state, &id)?;
    // A DryRun validates the request -- including that the association exists
    // -- and changes nothing, matching how the rest of EC2 treats one.
    if dry_run(req) {
        return Ok(Ec2Service::respond(action, &req.request_id, ""));
    }

    let creating = action == "CreateIpamRoutingPolicyRegistration";
    if creating && a.registrations.contains_key(&cidr) {
        return Err(invalid_parameter_value(format!(
            "A routing policy registration already exists for {cidr}"
        )));
    }
    if !creating && !a.registrations.contains_key(&cidr) {
        return Err(not_found(
            "InvalidIpamRoutingPolicyRegistration.NotFound",
            &cidr,
        ));
    }

    let delta_json = serde_json::json!({
        "action": if creating { "create" } else { "modify" },
        "cidr": cidr,
        "asns": asns,
        "maxLength": max_length,
    })
    .to_string();
    let delta_id = push_delta(a, delta_json);
    a.registrations.insert(
        cidr.clone(),
        IpamRoutingPolicyRegistration {
            cidr,
            asns,
            permit_more_specific_announcements: req
                .query_params
                .get("PermitMoreSpecificAnnouncements")
                .map(|v| v.eq_ignore_ascii_case("true")),
            max_length,
            description: req.query_params.get("Description").cloned(),
            latest_delta_id: delta_id.clone(),
            state: if creating {
                "create-complete".to_string()
            } else {
                "update-complete".to_string()
            },
        },
    );
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
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    let cidr = require(&req.query_params, "Cidr")?;
    let mut accounts = svc.state.write();
    let state = accounts.get_or_create(&req.account_id);
    let a = get_association(state, &id)?;
    // A DryRun validates the request -- including that the association exists
    // -- and changes nothing, matching how the rest of EC2 treats one.
    if dry_run(req) {
        return Ok(Ec2Service::respond(
            "DeleteIpamRoutingPolicyRegistration",
            &req.request_id,
            "",
        ));
    }
    if a.registrations.remove(&cidr).is_none() {
        return Err(not_found(
            "InvalidIpamRoutingPolicyRegistration.NotFound",
            &cidr,
        ));
    }
    let delta_json = serde_json::json!({ "action": "delete", "cidr": cidr }).to_string();
    push_delta(a, delta_json);
    let delta = a.deltas.last().expect("the delta was just pushed").clone();
    Ok(delta_response(
        "DeleteIpamRoutingPolicyRegistration",
        req,
        &delta,
    ))
}

/// A batch of registration changes, described by a JSON document rather than
/// indexed parameters. The whole batch lands as one delta.
pub(crate) fn batch_modify_ipam_routing_policy_registrations(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    let delta_json = require(&req.query_params, "DeltaJson")?;
    let parsed: serde_json::Value = serde_json::from_str(&delta_json)
        .map_err(|_| invalid_parameter_value("DeltaJson is not valid JSON"))?;

    let mut accounts = svc.state.write();
    let state = accounts.get_or_create(&req.account_id);
    let a = get_association(state, &id)?;
    // A DryRun validates the request -- including that the association exists
    // -- and changes nothing, matching how the rest of EC2 treats one.
    if dry_run(req) {
        return Ok(Ec2Service::respond(
            "BatchModifyIpamRoutingPolicyRegistrations",
            &req.request_id,
            "",
        ));
    }
    let delta_id = push_delta(a, delta_json.clone());

    // The document lists the registrations to add and the CIDRs to remove.
    if let Some(additions) = parsed.get("add").and_then(|v| v.as_array()) {
        for entry in additions {
            let Some(cidr) = entry.get("cidr").and_then(|v| v.as_str()) else {
                continue;
            };
            let asns: Vec<String> = entry
                .get("asns")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            a.registrations.insert(
                cidr.to_string(),
                IpamRoutingPolicyRegistration {
                    cidr: cidr.to_string(),
                    asns,
                    permit_more_specific_announcements: entry
                        .get("permitMoreSpecificAnnouncements")
                        .and_then(|v| v.as_bool()),
                    max_length: entry.get("maxLength").and_then(|v| v.as_i64()),
                    description: entry
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    latest_delta_id: delta_id.clone(),
                    state: "create-complete".to_string(),
                },
            );
        }
    }
    if let Some(removals) = parsed.get("remove").and_then(|v| v.as_array()) {
        for entry in removals {
            if let Some(cidr) = entry.as_str().or_else(|| entry.get("cidr")?.as_str()) {
                a.registrations.remove(cidr);
            }
        }
    }

    let delta = a.deltas.last().expect("the delta was just pushed").clone();
    Ok(delta_response(
        "BatchModifyIpamRoutingPolicyRegistrations",
        req,
        &delta,
    ))
}

pub(crate) fn get_ipam_routing_policy_registrations(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    mr(req)?;
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
    Ok(Ec2Service::respond(
        "GetIpamRoutingPolicyRegistrations",
        &req.request_id,
        &ec2_list("ipamRoutingPolicyRegistrationSet", &items),
    ))
}

pub(crate) fn get_ipam_routing_policy_registration_deltas(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    mr(req)?;
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
    Ok(Ec2Service::respond(
        "GetIpamRoutingPolicyRegistrationDeltas",
        &req.request_id,
        &ec2_list("ipamRoutingPolicyRegistrationDeltaSet", &items),
    ))
}

/// The route origin authorizations an association publishes: one per
/// registration and ASN pair, which is the shape a relying party consumes.
pub(crate) fn get_ipam_route_origin_authorizations(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    mr(req)?;
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
    Ok(Ec2Service::respond(
        "GetIpamRouteOriginAuthorizations",
        &req.request_id,
        &ec2_list("ipamRouteOriginAuthorizationSet", &items),
    ))
}

/// Per-ASN and per-CIDR views of what the registry has observed for an
/// association. Both derive from the registrations it publishes.
pub(crate) fn get_ipam_internet_registry_association_asns(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    mr(req)?;
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
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
        .map(|asn| ec2_elem("asn", asn) + &ec2_elem("lastObservedAt", &now))
        .collect();
    Ok(Ec2Service::respond(
        "GetIpamInternetRegistryAssociationAsns",
        &req.request_id,
        &ec2_list("ipamInternetRegistryAssociationAsnSet", &items),
    ))
}

pub(crate) fn get_ipam_internet_registry_association_cidrs(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    mr(req)?;
    let id = require(&req.query_params, "IpamInternetRegistryAssociationId")?;
    let accounts = svc.state.read();
    let a = accounts
        .get(&req.account_id)
        .and_then(|s| s.ipam_ir_associations.get(&id))
        .ok_or_else(|| association_not_found(&id))?;

    let now = now_rfc3339();
    let items: Vec<String> = a
        .registrations
        .keys()
        .map(|cidr| ec2_elem("cidr", cidr) + &ec2_elem("lastObservedAt", &now))
        .collect();
    Ok(Ec2Service::respond(
        "GetIpamInternetRegistryAssociationCidrs",
        &req.request_id,
        &ec2_list("ipamInternetRegistryAssociationCidrSet", &items),
    ))
}

/// Routes a resource discovery has seen in a region. fakecloud runs no BGP
/// collector, so the discovered set is what the account's own registrations
/// advertise there rather than a fabricated view of the internet.
pub(crate) fn get_ipam_discovered_routes(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    mr(req)?;
    let discovery_id = require(&req.query_params, "IpamResourceDiscoveryId")?;
    let resource_region = require(&req.query_params, "ResourceRegion")?;
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
    Ok(Ec2Service::respond(
        "GetIpamDiscoveredRoutes",
        &req.request_id,
        &ec2_list("ipamDiscoveredRouteSet", &items),
    ))
}

/// Route protection findings: a registration whose CIDR is authorized for its
/// ASNs is `valid`; one an association publishes with no ASN at all is
/// `unknown`, which is what an unsigned announcement looks like to RPKI.
pub(crate) fn get_ipam_route_protection_findings(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    mr(req)?;
    let ipam_id = require(&req.query_params, "IpamId")?;
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
    Ok(Ec2Service::respond(
        "GetIpamRouteProtectionFindings",
        &req.request_id,
        &format!(
            "{}{}",
            ec2_elem("ipamId", &ipam_id),
            ec2_list("routeProtectionFindingSet", &items)
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

    fn make_association(svc: &Ec2Service) -> String {
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
        // `IpamRpkiStrength` is `strict | permissive` — nothing else.
        assert!(b.contains("<rpkiStrength>strict</rpkiStrength>"), "{b}");
        assert!(!b.contains("strong"), "{b}");
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
        let b = body(
            get_ipam_routing_policy_registrations(
                &svc,
                &req(
                    "GetIpamRoutingPolicyRegistrations",
                    &[("IpamInternetRegistryAssociationId", &id)],
                ),
            )
            .unwrap(),
        );
        assert!(b.contains("192.0.2.0/24"), "{b}");
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
}
