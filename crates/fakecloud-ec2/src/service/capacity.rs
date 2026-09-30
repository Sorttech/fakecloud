//! On-demand capacity reservations, capacity reservation fleets, capacity
//! blocks, billing-owner transfer, and interruptible allocations.

use fakecloud_aws::ec2query::{ec2_elem, ec2_list, ec2_return};
use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::service::Ec2Service;
use crate::service_helpers::{
    ec2_arn, filter_value_matches, gen_id, incorrect_state, indexed_list, invalid_parameter_value,
    missing_parameter, not_found, paginate, parse_filters, require, validate_enum,
    validate_int_range, validate_length, validate_max_results, Filter,
};
use crate::state::{
    CapacityReservation, CapacityReservationAdjustment, CapacityReservationCommitment,
    CapacityReservationModificationQuote, Ec2State, Tag,
};
use chrono::{DateTime, Duration, Utc};

const FIXED_TIME: &str = "2024-01-01T00:00:00.000Z";

/// Cumulative start-date pushout allowed past a reservation's original start.
const MAX_PUSHOUT_DAYS: i64 = 30;
/// Date change quotes are valid for 24 hours...
const QUOTE_VALIDITY_HOURS: i64 = 24;
/// ...and always expire at least one hour before the reservation starts.
const QUOTE_START_MARGIN_HOURS: i64 = 1;

fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn parse_ts(v: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Parse an optional ISO 8601 timestamp parameter, rejecting a malformed one.
fn ts_param(req: &AwsRequest, key: &str) -> Result<Option<DateTime<Utc>>, AwsServiceError> {
    match req.query_params.get(key).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) => parse_ts(v)
            .map(Some)
            .ok_or_else(|| invalid_parameter_value(format!("Invalid value '{v}' for {key}"))),
    }
}

/// The reservation's state as of `now`: a future-dated reservation is
/// `scheduled` until its start date arrives, then it is delivered (`active`).
fn current_state(r: &CapacityReservation, now: DateTime<Utc>) -> String {
    if r.state == "scheduled"
        && r.start_date
            .as_deref()
            .and_then(parse_ts)
            .is_some_and(|s| s <= now)
    {
        return "active".to_string();
    }
    r.state.clone()
}

fn commitment_xml(c: &CapacityReservationCommitment) -> String {
    format!(
        "<commitmentInfo><committedInstanceCount>{}</committedInstanceCount>{}<commitmentDuration>{}</commitmentDuration></commitmentInfo>",
        c.committed_instance_count,
        ec2_elem("commitmentEndDate", &c.commitment_end_date),
        c.commitment_duration,
    )
}

fn adjustment_xml(a: &CapacityReservationAdjustment) -> String {
    let mut out = String::from("<adjustmentDetails>");
    if let Some(v) = &a.start_date {
        out.push_str(&ec2_elem("startDate", v));
    }
    if let Some(v) = &a.end_date {
        out.push_str(&ec2_elem("endDate", v));
    }
    if let Some(v) = &a.commitment_end_date {
        out.push_str(&ec2_elem("commitmentEndDate", v));
    }
    out.push_str(&ec2_elem("endDateType", &a.end_date_type));
    if let Some(v) = a.commitment_duration {
        out.push_str(&format!("<commitmentDuration>{v}</commitmentDuration>"));
    }
    out.push_str("</adjustmentDetails>");
    out
}

/// `adjustmentStatus` + `adjustmentDetails`, shared by the reservation render
/// and the `ModifyCapacityReservation` result.
fn adjustment_fields_xml(r: &CapacityReservation) -> String {
    let mut out = String::new();
    if let Some(s) = &r.adjustment_status {
        out.push_str(&ec2_elem("adjustmentStatus", s));
    }
    if let Some(a) = &r.adjustment_details {
        out.push_str(&adjustment_xml(a));
    }
    out
}

const INSTANCE_PLATFORMS: &[&str] = &[
    "Linux/UNIX",
    "Red Hat Enterprise Linux",
    "SUSE Linux",
    "Windows",
    "Windows with SQL Server",
    "Windows with SQL Server Enterprise",
    "Windows with SQL Server Standard",
    "Windows with SQL Server Web",
    "Linux with SQL Server Standard",
    "Linux with SQL Server Web",
    "Linux with SQL Server Enterprise",
    "RHEL with SQL Server Standard",
    "RHEL with SQL Server Enterprise",
    "RHEL with SQL Server Web",
    "RHEL with HA",
    "RHEL with HA and SQL Server Standard",
    "RHEL with HA and SQL Server Enterprise",
    "Ubuntu Pro",
];

fn cr_xml(r: &CapacityReservation, tags: &[Tag], owner: &str) -> String {
    let mut extra = String::new();
    if let Some(e) = &r.end_date {
        extra.push_str(&ec2_elem("endDate", e));
    }
    if let Some(c) = &r.commitment {
        extra.push_str(&commitment_xml(c));
    }
    if let Some(o) = &r.original_start_date {
        extra.push_str(&ec2_elem("originalStartDate", o));
    }
    extra.push_str(&adjustment_fields_xml(r));
    format!(
        "{}{}{}{}{}{}{}<totalInstanceCount>{}</totalInstanceCount><availableInstanceCount>{}</availableInstanceCount>\
         <ebsOptimized>{}</ebsOptimized><ephemeralStorage>{}</ephemeralStorage>{}{}{}{}{}{}{}",
        ec2_elem("capacityReservationId", &r.id),
        ec2_elem("ownerId", owner),
        ec2_elem("capacityReservationArn", &ec2_arn(&region_of(r), owner, &format!("capacity-reservation/{}", r.id))),
        ec2_elem("instanceType", &r.instance_type),
        ec2_elem("instancePlatform", &r.instance_platform),
        ec2_elem("availabilityZone", &r.availability_zone),
        ec2_elem("tenancy", &r.tenancy),
        r.total_instance_count,
        r.available_instance_count,
        r.ebs_optimized,
        r.ephemeral_storage,
        ec2_elem("state", &current_state(r, Utc::now())),
        ec2_elem("endDateType", &r.end_date_type),
        ec2_elem("instanceMatchCriteria", &r.instance_match_criteria),
        ec2_elem("createDate", FIXED_TIME),
        ec2_elem("startDate", r.start_date.as_deref().unwrap_or(FIXED_TIME)),
        super::tags::tag_set_xml(tags),
        extra,
    )
}

/// `InvalidCapacityReservationId.NotFound` — the reservation does not exist.
fn cr_not_found(id: &str) -> AwsServiceError {
    AwsServiceError::aws_error(
        http::StatusCode::BAD_REQUEST,
        "InvalidCapacityReservationId.NotFound",
        format!("The capacity reservation ID '{id}' does not exist"),
    )
}

fn region_of(r: &CapacityReservation) -> String {
    r.availability_zone
        .trim_end_matches(|c: char| c.is_alphabetic())
        .to_string()
}

pub(crate) fn create_capacity_reservation(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let instance_type = require(&req.query_params, "InstanceType")?;
    let platform = require(&req.query_params, "InstancePlatform")?;
    let count: i64 = require(&req.query_params, "InstanceCount")?
        .parse()
        .unwrap_or(1);
    validate_enum(&req.query_params, "InstancePlatform", INSTANCE_PLATFORMS)?;
    validate_enum(&req.query_params, "Tenancy", &["default", "dedicated"])?;
    validate_enum(&req.query_params, "EndDateType", &["unlimited", "limited"])?;
    validate_enum(
        &req.query_params,
        "InstanceMatchCriteria",
        &["open", "targeted"],
    )?;
    validate_enum(
        &req.query_params,
        "DeliveryPreference",
        &["fixed", "incremental"],
    )?;
    validate_int_range(&req.query_params, "CommitmentDuration", 1, 200_000_000)?;
    let now = Utc::now();
    let start = ts_param(req, "StartDate")?;
    let end = ts_param(req, "EndDate")?;
    // A reservation whose StartDate lies in the future is future-dated: it is
    // `scheduled` until delivered, carries its original start date, and any
    // CommitmentDuration runs from that start.
    let future_start = start.filter(|s| *s > now);
    let commitment = match (
        future_start,
        req.query_params
            .get("CommitmentDuration")
            .and_then(|v| v.parse::<i64>().ok()),
    ) {
        (Some(s), Some(secs)) => Some(CapacityReservationCommitment {
            committed_instance_count: count,
            commitment_duration: secs,
            commitment_end_date: fmt_ts(s + Duration::seconds(secs)),
        }),
        _ => None,
    };
    let id = gen_id("cr");
    let r = CapacityReservation {
        id: id.clone(),
        instance_type,
        instance_platform: platform,
        availability_zone: req
            .query_params
            .get("AvailabilityZone")
            .cloned()
            .unwrap_or_else(|| {
                format!(
                    "{}a",
                    if req.region.is_empty() {
                        "us-east-1"
                    } else {
                        &req.region
                    }
                )
            }),
        tenancy: req
            .query_params
            .get("Tenancy")
            .cloned()
            .unwrap_or_else(|| "default".to_string()),
        total_instance_count: count,
        available_instance_count: count,
        state: if future_start.is_some() {
            "scheduled"
        } else {
            "active"
        }
        .to_string(),
        start_date: start.map(fmt_ts),
        end_date: end.map(fmt_ts),
        original_start_date: future_start.map(fmt_ts),
        commitment,
        adjustment_status: None,
        adjustment_details: None,
        ebs_optimized: req
            .query_params
            .get("EbsOptimized")
            .is_some_and(|v| v == "true"),
        ephemeral_storage: req
            .query_params
            .get("EphemeralStorage")
            .is_some_and(|v| v == "true"),
        end_date_type: req
            .query_params
            .get("EndDateType")
            .cloned()
            .unwrap_or_else(|| "unlimited".to_string()),
        instance_match_criteria: req
            .query_params
            .get("InstanceMatchCriteria")
            .cloned()
            .unwrap_or_else(|| "open".to_string()),
    };
    let owner = req.account_id.clone();
    let tags = {
        let mut accounts = svc.state.write();
        let state = accounts.get_or_create(&req.account_id);
        crate::service::tags::apply_tag_specifications(
            state,
            &req.query_params,
            &id,
            "capacity-reservation",
        );
        let t = state.tags_for(&id).to_vec();
        state.capacity_reservations.insert(id.clone(), r.clone());
        t
    };
    Ok(Ec2Service::respond(
        "CreateCapacityReservation",
        &req.request_id,
        &format!(
            "<capacityReservation>{}</capacityReservation>",
            cr_xml(&r, &tags, &owner)
        ),
    ))
}

pub(crate) fn cancel_capacity_reservation(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let id = require(&req.query_params, "CapacityReservationId")?;
    validate_enum(
        &req.query_params,
        "ApplyCancellationCharges",
        &["commitment-wind-down"],
    )?;
    {
        let mut accounts = svc.state.write();
        let r = accounts
            .get_or_create(&req.account_id)
            .capacity_reservations
            .get_mut(&id)
            .ok_or_else(|| cr_not_found(&id))?;
        r.state = "cancelled".to_string();
    }
    Ok(Ec2Service::respond(
        "CancelCapacityReservation",
        &req.request_id,
        &ec2_return(true),
    ))
}

pub(crate) fn describe_capacity_reservations(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    validate_max_results(&req.query_params, 1, 1000)?;
    let wanted = indexed_list(&req.query_params, "CapacityReservationId");
    let owner = req.account_id.clone();
    let accounts = svc.state.read();
    let empty = Ec2State::new(&req.account_id, &req.region);
    let state = accounts.get(&req.account_id).unwrap_or(&empty);
    let mut items: Vec<String> = state
        .capacity_reservations
        .values()
        .filter(|r| wanted.is_empty() || wanted.contains(&r.id))
        .map(|r| cr_xml(r, state.tags_for(&r.id), &owner))
        .collect();
    items.sort();
    Ok(Ec2Service::respond(
        "DescribeCapacityReservations",
        &req.request_id,
        &ec2_list("capacityReservationSet", &items),
    ))
}

pub(crate) fn modify_capacity_reservation(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let id = require(&req.query_params, "CapacityReservationId")?;
    validate_enum(&req.query_params, "EndDateType", &["unlimited", "limited"])?;
    validate_enum(
        &req.query_params,
        "InstanceMatchCriteria",
        &["open", "targeted"],
    )?;
    let end = ts_param(req, "EndDate")?;
    let start = ts_param(req, "StartDate")?;
    let quote_id = req
        .query_params
        .get("QuoteId")
        .filter(|v| !v.is_empty())
        .cloned();
    let accept_terms = req
        .query_params
        .get("AcceptModificationTerms")
        .is_some_and(|v| v == "true");
    // A start-date change is only ever applied through an accepted quote.
    if start.is_some() && quote_id.is_none() {
        return Err(missing_parameter("QuoteId"));
    }
    if quote_id.is_some() && !accept_terms {
        return Err(invalid_parameter_value(
            "AcceptModificationTerms must be true to apply the modification described by QuoteId",
        ));
    }
    let now = Utc::now();
    let result = {
        let mut accounts = svc.state.write();
        let state = accounts.get_or_create(&req.account_id);
        let r = state
            .capacity_reservations
            .get_mut(&id)
            .ok_or_else(|| cr_not_found(&id))?;
        let quoted_start = match &quote_id {
            None => None,
            Some(qid) => {
                let q = state
                    .capacity_reservation_modification_quotes
                    .get(qid)
                    .ok_or_else(|| quote_not_found(qid))?;
                if q.capacity_reservation_id != id {
                    return Err(invalid_parameter_value(format!(
                        "The quote '{qid}' is not for Capacity Reservation '{id}'"
                    )));
                }
                if quote_state(q, now) != "active" {
                    return Err(incorrect_state(format!(
                        "The quote '{qid}' is expired or has already been used"
                    )));
                }
                let new_start = parse_ts(&q.new_start_date).unwrap_or(now);
                // Quote dates are stored at millisecond precision; compare the
                // caller's StartDate at the same precision.
                if start.is_some_and(|s| fmt_ts(s) != q.new_start_date) {
                    return Err(invalid_parameter_value(format!(
                        "StartDate does not match the start date quoted by '{qid}'"
                    )));
                }
                Some(new_start)
            }
        };
        if let Some(new_start) = quoted_start {
            if current_state(r, now) != "scheduled" {
                return Err(incorrect_state(format!(
                    "The start date of Capacity Reservation '{id}' can't be changed because it has already been delivered"
                )));
            }
            r.start_date = Some(fmt_ts(new_start));
            if let Some(c) = r.commitment.as_mut() {
                c.commitment_end_date =
                    fmt_ts(new_start + Duration::seconds(c.commitment_duration));
            }
        }
        if let Some(e) = end {
            r.end_date = Some(fmt_ts(e));
        }
        if let Some(c) = req
            .query_params
            .get("InstanceCount")
            .and_then(|v| v.parse().ok())
        {
            r.total_instance_count = c;
            r.available_instance_count = c;
        }
        // EndDateType and InstanceMatchCriteria are validated above but were
        // never persisted -> DescribeCapacityReservations read back the
        // create-time values and aws_ec2_capacity_reservation drifted. Both are
        // modifiable per AWS; honor them.
        if let Some(t) = req.query_params.get("EndDateType") {
            r.end_date_type = t.clone();
            // An unlimited reservation has no end date.
            if t == "unlimited" {
                r.end_date = None;
            }
        }
        if let Some(m) = req.query_params.get("InstanceMatchCriteria") {
            r.instance_match_criteria = m.clone();
        }
        // Modifications apply synchronously here, so the most recent one is
        // always `applied` and the details are the resulting configuration.
        r.adjustment_status = Some("applied".to_string());
        r.adjustment_details = Some(CapacityReservationAdjustment {
            start_date: r.start_date.clone(),
            end_date: r.end_date.clone(),
            commitment_end_date: r.commitment.as_ref().map(|c| c.commitment_end_date.clone()),
            end_date_type: r.end_date_type.clone(),
            commitment_duration: r.commitment.as_ref().map(|c| c.commitment_duration),
        });
        let result = adjustment_fields_xml(r);
        // Accepting a quote changes the reservation the other outstanding
        // quotes were priced against, so every quote for it is spent.
        if quoted_start.is_some() {
            for q in state
                .capacity_reservation_modification_quotes
                .values_mut()
                .filter(|q| q.capacity_reservation_id == id)
            {
                q.used = true;
            }
        }
        result
    };
    Ok(Ec2Service::respond(
        "ModifyCapacityReservation",
        &req.request_id,
        &format!("{}{result}", ec2_return(true)),
    ))
}

// ---- date change quotes ----

fn quote_not_found(id: &str) -> AwsServiceError {
    not_found("InvalidCapacityReservationModificationQuoteId.NotFound", id)
}

/// `active` until the quote is used or reaches its expiration time.
fn quote_state(q: &CapacityReservationModificationQuote, now: DateTime<Utc>) -> &'static str {
    let expired = q.used || parse_ts(&q.expiration_time).is_none_or(|e| now >= e);
    if expired {
        "expired"
    } else {
        "active"
    }
}

fn quote_xml(q: &CapacityReservationModificationQuote, tags: &[Tag], now: DateTime<Utc>) -> String {
    let mut update = ec2_elem("newStartDate", &q.new_start_date);
    if let Some(v) = &q.new_commitment_end_date {
        update = format!("{}{update}", ec2_elem("newCommitmentEndDate", v));
    }
    if let Some(v) = q.new_commitment_duration {
        update.push_str(&format!(
            "<newCommitmentDuration>{v}</newCommitmentDuration>"
        ));
    }
    format!(
        "{}{}{}{}{}<currentConfiguration><instanceCount>{}</instanceCount>{}{}{}</currentConfiguration>\
         <modificationTerms><reservationUpdate>{update}</reservationUpdate></modificationTerms>{}",
        ec2_elem("capacityReservationModificationQuoteId", &q.id),
        ec2_elem("capacityReservationId", &q.capacity_reservation_id),
        ec2_elem("createTime", &q.create_time),
        ec2_elem("expirationTime", &q.expiration_time),
        ec2_elem("quoteState", quote_state(q, now)),
        q.current_instance_count,
        ec2_elem("reservationState", &q.current_reservation_state),
        ec2_elem("startDate", &q.current_start_date),
        ec2_elem("originalStartDate", &q.original_start_date),
        super::tags::tag_set_xml(tags),
    )
}

pub(crate) fn create_capacity_reservation_date_change_quote(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let id = require(&req.query_params, "CapacityReservationId")?;
    let new_start =
        ts_param(req, "NewStartDate")?.ok_or_else(|| missing_parameter("NewStartDate"))?;
    let client_token = req
        .query_params
        .get("ClientToken")
        .filter(|v| !v.is_empty())
        .cloned();
    let now = Utc::now();
    let mut accounts = svc.state.write();
    let state = accounts.get_or_create(&req.account_id);
    // Idempotent retry: the same ClientToken returns the original quote.
    if let Some(token) = &client_token {
        if let Some(q) = state
            .capacity_reservation_modification_quotes
            .values()
            .find(|q| q.client_token.as_deref() == Some(token.as_str()))
        {
            if q.capacity_reservation_id != id || q.new_start_date != fmt_ts(new_start) {
                return Err(AwsServiceError::aws_error(
                    http::StatusCode::BAD_REQUEST,
                    "IdempotentParameterMismatch",
                    "The client token has already been used with different parameters",
                ));
            }
            let body = format!(
                "<capacityReservationModificationQuote>{}</capacityReservationModificationQuote>",
                quote_xml(q, state.tags_for(&q.id), now)
            );
            return Ok(Ec2Service::respond(
                "CreateCapacityReservationDateChangeQuote",
                &req.request_id,
                &body,
            ));
        }
    }
    let r = state
        .capacity_reservations
        .get(&id)
        .ok_or_else(|| cr_not_found(&id))?;
    let reservation_state = current_state(r, now);
    let current_start = r.start_date.as_deref().and_then(parse_ts);
    let (Some(current_start), "scheduled") = (current_start, reservation_state.as_str()) else {
        return Err(incorrect_state(format!(
            "Capacity Reservation '{id}' is not a future-dated Capacity Reservation that has not yet been delivered"
        )));
    };
    let original_start = r
        .original_start_date
        .as_deref()
        .and_then(parse_ts)
        .unwrap_or(current_start);
    if new_start <= current_start {
        return Err(invalid_parameter_value(
            "NewStartDate must be later than the current start date of the Capacity Reservation",
        ));
    }
    if new_start > original_start + Duration::days(MAX_PUSHOUT_DAYS) {
        return Err(invalid_parameter_value(format!(
            "NewStartDate exceeds the {MAX_PUSHOUT_DAYS}-day limit on start date pushouts from the original start date"
        )));
    }
    let expiration = (now + Duration::hours(QUOTE_VALIDITY_HOURS))
        .min(current_start - Duration::hours(QUOTE_START_MARGIN_HOURS));
    if expiration <= now {
        return Err(incorrect_state(format!(
            "Capacity Reservation '{id}' starts too soon for its start date to be changed"
        )));
    }
    let quote = CapacityReservationModificationQuote {
        id: gen_id("crmq"),
        capacity_reservation_id: id.clone(),
        create_time: fmt_ts(now),
        expiration_time: fmt_ts(expiration),
        used: false,
        client_token,
        current_instance_count: r.total_instance_count,
        current_reservation_state: reservation_state,
        current_start_date: fmt_ts(current_start),
        original_start_date: fmt_ts(original_start),
        new_start_date: fmt_ts(new_start),
        new_commitment_end_date: r
            .commitment
            .as_ref()
            .map(|c| fmt_ts(new_start + Duration::seconds(c.commitment_duration))),
        new_commitment_duration: r.commitment.as_ref().map(|c| c.commitment_duration),
    };
    crate::service::tags::apply_tag_specifications(
        state,
        &req.query_params,
        &quote.id,
        "capacity-reservation-modification-quote",
    );
    let body = format!(
        "<capacityReservationModificationQuote>{}</capacityReservationModificationQuote>",
        quote_xml(&quote, state.tags_for(&quote.id), now)
    );
    state
        .capacity_reservation_modification_quotes
        .insert(quote.id.clone(), quote);
    Ok(Ec2Service::respond(
        "CreateCapacityReservationDateChangeQuote",
        &req.request_id,
        &body,
    ))
}

fn quote_matches(
    q: &CapacityReservationModificationQuote,
    tags: &[Tag],
    filters: &[Filter],
    now: DateTime<Utc>,
) -> bool {
    filters.iter().all(|f| {
        let candidates: Vec<String> = match f.name.as_str() {
            "capacity-reservation-id" => vec![q.capacity_reservation_id.clone()],
            "capacity-reservation-modification-quote-id" => vec![q.id.clone()],
            "quote-state" => vec![quote_state(q, now).to_string()],
            "tag-key" => tags.iter().map(|t| t.key.clone()).collect(),
            name => match name.strip_prefix("tag:") {
                Some(key) => tags
                    .iter()
                    .filter(|t| t.key == key)
                    .map(|t| t.value.clone())
                    .collect(),
                None => return false,
            },
        };
        f.values
            .iter()
            .any(|v| candidates.iter().any(|c| filter_value_matches(v, c)))
    })
}

pub(crate) fn describe_capacity_reservation_date_change_quotes(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    validate_max_results(&req.query_params, 1, 1000)?;
    let wanted = indexed_list(&req.query_params, "CapacityReservationModificationQuoteId");
    let filters = parse_filters(&req.query_params);
    let now = Utc::now();
    let accounts = svc.state.read();
    let empty = Ec2State::new(&req.account_id, &req.region);
    let state = accounts.get(&req.account_id).unwrap_or(&empty);
    let quotes = &state.capacity_reservation_modification_quotes;
    if let Some(missing) = wanted.iter().find(|id| !quotes.contains_key(*id)) {
        return Err(quote_not_found(missing));
    }
    // BTreeMap iteration is already id-ordered, giving stable pagination.
    let items: Vec<String> = quotes
        .values()
        .filter(|q| wanted.is_empty() || wanted.contains(&q.id))
        .filter(|q| quote_matches(q, state.tags_for(&q.id), &filters, now))
        .map(|q| quote_xml(q, state.tags_for(&q.id), now))
        .collect();
    let max_results = req
        .query_params
        .get("MaxResults")
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<usize>().ok());
    let next_token = req.query_params.get("NextToken").map(String::as_str);
    let (page, token) = paginate(&items, next_token, max_results);
    let body = format!(
        "{}{}",
        ec2_list("capacityReservationModificationQuoteSet", &page),
        token.map(|t| ec2_elem("nextToken", &t)).unwrap_or_default(),
    );
    Ok(Ec2Service::respond(
        "DescribeCapacityReservationDateChangeQuotes",
        &req.request_id,
        &body,
    ))
}

pub(crate) fn get_capacity_reservation_usage(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let id = require(&req.query_params, "CapacityReservationId")?;
    validate_max_results(&req.query_params, 1, 1000)?;
    let accounts = svc.state.read();
    let r = accounts
        .get(&req.account_id)
        .and_then(|s| s.capacity_reservations.get(&id).cloned());
    let (itype, total, avail) = r
        .map(|r| {
            (
                r.instance_type,
                r.total_instance_count,
                r.available_instance_count,
            )
        })
        .unwrap_or_else(|| ("t3.micro".to_string(), 1, 1));
    let body = format!(
        "{}{}<totalInstanceCount>{}</totalInstanceCount><availableInstanceCount>{}</availableInstanceCount>{}{}",
        ec2_elem("capacityReservationId", &id),
        ec2_elem("instanceType", &itype),
        total,
        avail,
        ec2_elem("state", "active"),
        ec2_list("instanceUsageSet", &[]),
    );
    Ok(Ec2Service::respond(
        "GetCapacityReservationUsage",
        &req.request_id,
        &body,
    ))
}

// ---- capacity reservation fleets ----

pub(crate) fn create_capacity_reservation_fleet(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    require(&req.query_params, "TotalTargetCapacity")?;
    validate_enum(&req.query_params, "Tenancy", &["default"])?;
    validate_enum(&req.query_params, "InstanceMatchCriteria", &["open"])?;
    let id = gen_id("crf");
    let cap = req
        .query_params
        .get("TotalTargetCapacity")
        .cloned()
        .unwrap_or_else(|| "1".to_string());
    {
        let mut accounts = svc.state.write();
        // The map value stores the fleet's TotalTargetCapacity so Describe /
        // Modify can round-trip it rather than reporting a hardcoded 1.
        accounts
            .get_or_create(&req.account_id)
            .capacity_reservation_fleets
            .insert(id.clone(), cap.clone());
    }
    let body = format!(
        "{}{}<totalTargetCapacity>{}</totalTargetCapacity><totalFulfilledCapacity>{}</totalFulfilledCapacity>{}{}{}{}",
        ec2_elem("capacityReservationFleetId", &id),
        ec2_elem("state", "active"),
        cap, cap,
        ec2_elem("instanceMatchCriteria", "open"),
        ec2_elem("allocationStrategy", "prioritized"),
        ec2_elem("tenancy", "default"),
        ec2_elem("createTime", FIXED_TIME),
    );
    Ok(Ec2Service::respond(
        "CreateCapacityReservationFleet",
        &req.request_id,
        &body,
    ))
}

pub(crate) fn describe_capacity_reservation_fleets(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    validate_max_results(&req.query_params, 1, 100)?;
    let wanted = indexed_list(&req.query_params, "CapacityReservationFleetId");
    let accounts = svc.state.read();
    let empty = Ec2State::new(&req.account_id, &req.region);
    let state = accounts.get(&req.account_id).unwrap_or(&empty);
    let mut items: Vec<String> = state
        .capacity_reservation_fleets
        .iter()
        .filter(|(id, _)| wanted.is_empty() || wanted.contains(id))
        .map(|(id, cap)| {
            format!(
                "{}{}<totalTargetCapacity>{}</totalTargetCapacity>",
                ec2_elem("capacityReservationFleetId", id),
                ec2_elem("state", "active"),
                cap,
            )
        })
        .collect();
    items.sort();
    Ok(Ec2Service::respond(
        "DescribeCapacityReservationFleets",
        &req.request_id,
        &ec2_list("capacityReservationFleetSet", &items),
    ))
}

pub(crate) fn cancel_capacity_reservation_fleets(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let ids = indexed_list(&req.query_params, "CapacityReservationFleetId");
    {
        let mut accounts = svc.state.write();
        let state = accounts.get_or_create(&req.account_id);
        for id in &ids {
            state.capacity_reservation_fleets.remove(id);
        }
    }
    let items: Vec<String> = ids
        .iter()
        .map(|id| format!("{}<currentFleetState>cancelled</currentFleetState><previousFleetState>active</previousFleetState>", ec2_elem("capacityReservationFleetId", id)))
        .collect();
    let body = format!(
        "{}{}",
        ec2_list("successfulFleetCancellationSet", &items),
        ec2_list("failedFleetCancellationSet", &[])
    );
    Ok(Ec2Service::respond(
        "CancelCapacityReservationFleets",
        &req.request_id,
        &body,
    ))
}

pub(crate) fn modify_capacity_reservation_fleet(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let id = require(&req.query_params, "CapacityReservationFleetId")?;
    {
        let mut accounts = svc.state.write();
        let state = accounts.get_or_create(&req.account_id);
        let cap = state
            .capacity_reservation_fleets
            .get_mut(&id)
            .ok_or_else(|| {
                AwsServiceError::aws_error(
                    http::StatusCode::BAD_REQUEST,
                    "InvalidCapacityReservationFleetId.NotFound",
                    format!("The capacity reservation fleet ID '{id}' does not exist"),
                )
            })?;
        if let Some(tc) = req.query_params.get("TotalTargetCapacity") {
            *cap = tc.clone();
        }
    }
    Ok(Ec2Service::respond(
        "ModifyCapacityReservationFleet",
        &req.request_id,
        &ec2_return(true),
    ))
}

pub(crate) fn modify_instance_capacity_reservation_attributes(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    require(&req.query_params, "InstanceId")?;
    Ok(Ec2Service::respond(
        "ModifyInstanceCapacityReservationAttributes",
        &req.request_id,
        &ec2_return(true),
    ))
}

// ---- split / move ----

fn synth_cr_xml(id: &str, owner: &str, count: i64) -> String {
    let r = CapacityReservation {
        id: id.to_string(),
        instance_type: "t3.micro".to_string(),
        instance_platform: "Linux/UNIX".to_string(),
        availability_zone: "us-east-1a".to_string(),
        tenancy: "default".to_string(),
        total_instance_count: count,
        available_instance_count: count,
        state: "active".to_string(),
        end_date_type: "unlimited".to_string(),
        instance_match_criteria: "open".to_string(),
        ..Default::default()
    };
    cr_xml(&r, &[], owner)
}

pub(crate) fn create_capacity_reservation_by_splitting(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let src = require(&req.query_params, "SourceCapacityReservationId")?;
    let count: i64 = require(&req.query_params, "InstanceCount")?
        .parse()
        .unwrap_or(1);
    let owner = req.account_id.clone();
    let dest = gen_id("cr");
    {
        let mut accounts = svc.state.write();
        let state = accounts.get_or_create(&req.account_id);
        if let Some(r) = state.capacity_reservations.get(&src).cloned() {
            let mut d = r.clone();
            d.id = dest.clone();
            d.total_instance_count = count;
            d.available_instance_count = count;
            state.capacity_reservations.insert(dest.clone(), d);
        }
    }
    let body = format!(
        "<sourceCapacityReservation>{}</sourceCapacityReservation><destinationCapacityReservation>{}</destinationCapacityReservation><instanceCount>{}</instanceCount>",
        synth_cr_xml(&src, &owner, count),
        synth_cr_xml(&dest, &owner, count),
        count,
    );
    Ok(Ec2Service::respond(
        "CreateCapacityReservationBySplitting",
        &req.request_id,
        &body,
    ))
}

pub(crate) fn move_capacity_reservation_instances(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let src = require(&req.query_params, "SourceCapacityReservationId")?;
    let dest = require(&req.query_params, "DestinationCapacityReservationId")?;
    let count: i64 = require(&req.query_params, "InstanceCount")?
        .parse()
        .unwrap_or(1);
    let owner = req.account_id.clone();
    let body = format!(
        "<sourceCapacityReservation>{}</sourceCapacityReservation><destinationCapacityReservation>{}</destinationCapacityReservation><instanceCount>{}</instanceCount>",
        synth_cr_xml(&src, &owner, count),
        synth_cr_xml(&dest, &owner, count),
        count,
    );
    Ok(Ec2Service::respond(
        "MoveCapacityReservationInstances",
        &req.request_id,
        &body,
    ))
}

// ---- billing owner transfer ----

pub(crate) fn describe_capacity_reservation_billing_requests(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    require(&req.query_params, "Role")?;
    validate_enum(
        &req.query_params,
        "Role",
        &["odcr-owner", "unused-reservation-billing-owner"],
    )?;
    validate_max_results(&req.query_params, 1, 1000)?;
    Ok(Ec2Service::respond(
        "DescribeCapacityReservationBillingRequests",
        &req.request_id,
        &ec2_list("capacityReservationBillingRequestSet", &[]),
    ))
}

fn cr_ack(req: &AwsRequest, action: &str, extra: &[&str]) -> Result<AwsResponse, AwsServiceError> {
    require(&req.query_params, "CapacityReservationId")?;
    for k in extra {
        require(&req.query_params, k)?;
    }
    // UnusedReservationBillingOwnerId is a 12-digit account id when present.
    validate_length(&req.query_params, "UnusedReservationBillingOwnerId", 12, 12)?;
    Ok(Ec2Service::respond(
        action,
        &req.request_id,
        &ec2_return(true),
    ))
}

pub(crate) fn associate_capacity_reservation_billing_owner(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    cr_ack(
        req,
        "AssociateCapacityReservationBillingOwner",
        &["UnusedReservationBillingOwnerId"],
    )
}
pub(crate) fn disassociate_capacity_reservation_billing_owner(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    cr_ack(
        req,
        "DisassociateCapacityReservationBillingOwner",
        &["UnusedReservationBillingOwnerId"],
    )
}
pub(crate) fn accept_capacity_reservation_billing_ownership(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    cr_ack(req, "AcceptCapacityReservationBillingOwnership", &[])
}
pub(crate) fn reject_capacity_reservation_billing_ownership(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    cr_ack(req, "RejectCapacityReservationBillingOwnership", &[])
}

// ---- capacity blocks ----

pub(crate) fn describe_capacity_block_offerings(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    require(&req.query_params, "CapacityDurationHours")?;
    validate_max_results(&req.query_params, 1, 1000)?;
    Ok(Ec2Service::respond(
        "DescribeCapacityBlockOfferings",
        &req.request_id,
        &ec2_list("capacityBlockOfferingSet", &[]),
    ))
}

pub(crate) fn describe_capacity_blocks(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    validate_max_results(&req.query_params, 1, 1000)?;
    Ok(Ec2Service::respond(
        "DescribeCapacityBlocks",
        &req.request_id,
        &ec2_list("capacityBlockSet", &[]),
    ))
}

pub(crate) fn purchase_capacity_block(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    require(&req.query_params, "CapacityBlockOfferingId")?;
    require(&req.query_params, "InstancePlatform")?;
    validate_enum(&req.query_params, "InstancePlatform", INSTANCE_PLATFORMS)?;
    let owner = req.account_id.clone();
    let id = gen_id("cr");
    {
        let mut accounts = svc.state.write();
        accounts
            .get_or_create(&req.account_id)
            .capacity_reservations
            .insert(
                id.clone(),
                CapacityReservation {
                    id: id.clone(),
                    instance_type: "p5.48xlarge".to_string(),
                    instance_platform: req
                        .query_params
                        .get("InstancePlatform")
                        .cloned()
                        .unwrap_or_else(|| "Linux/UNIX".to_string()),
                    availability_zone: "us-east-1a".to_string(),
                    tenancy: "default".to_string(),
                    total_instance_count: 1,
                    available_instance_count: 1,
                    state: "active".to_string(),
                    end_date_type: "limited".to_string(),
                    instance_match_criteria: "targeted".to_string(),
                    ..Default::default()
                },
            );
    }
    let body = format!(
        "<capacityReservation>{}</capacityReservation>{}",
        synth_cr_xml(&id, &owner, 1),
        ec2_list("capacityBlockSet", &[])
    );
    Ok(Ec2Service::respond(
        "PurchaseCapacityBlock",
        &req.request_id,
        &body,
    ))
}

pub(crate) fn describe_capacity_block_status(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    validate_max_results(&req.query_params, 1, 1000)?;
    Ok(Ec2Service::respond(
        "DescribeCapacityBlockStatus",
        &req.request_id,
        &ec2_list("capacityBlockStatusSet", &[]),
    ))
}

pub(crate) fn describe_capacity_block_extension_history(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    validate_max_results(&req.query_params, 1, 1000)?;
    Ok(Ec2Service::respond(
        "DescribeCapacityBlockExtensionHistory",
        &req.request_id,
        &ec2_list("capacityBlockExtensionSet", &[]),
    ))
}

pub(crate) fn describe_capacity_block_extension_offerings(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    require(&req.query_params, "CapacityBlockExtensionDurationHours")?;
    require(&req.query_params, "CapacityReservationId")?;
    validate_max_results(&req.query_params, 1, 1000)?;
    Ok(Ec2Service::respond(
        "DescribeCapacityBlockExtensionOfferings",
        &req.request_id,
        &ec2_list("capacityBlockExtensionOfferingSet", &[]),
    ))
}

pub(crate) fn purchase_capacity_block_extension(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    require(&req.query_params, "CapacityBlockExtensionOfferingId")?;
    require(&req.query_params, "CapacityReservationId")?;
    Ok(Ec2Service::respond(
        "PurchaseCapacityBlockExtension",
        &req.request_id,
        &ec2_list("capacityBlockExtensionSet", &[]),
    ))
}

// ---- cancellation quotes ----

pub(crate) fn describe_capacity_reservation_topology(
    svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    validate_max_results(&req.query_params, 1, 10)?;
    let _ = svc;
    Ok(Ec2Service::respond(
        "DescribeCapacityReservationTopology",
        &req.request_id,
        &ec2_list("capacityReservationSet", &[]),
    ))
}

// ---- interruptible allocations ----

pub(crate) fn create_interruptible_capacity_reservation_allocation(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let id = require(&req.query_params, "CapacityReservationId")?;
    let count = require(&req.query_params, "InstanceCount")?;
    let body = format!(
        "{}<targetInstanceCount>{}</targetInstanceCount><status>active</status><interruptionType>spot</interruptionType>",
        ec2_elem("sourceCapacityReservationId", &id),
        count,
    );
    Ok(Ec2Service::respond(
        "CreateInterruptibleCapacityReservationAllocation",
        &req.request_id,
        &body,
    ))
}

pub(crate) fn update_interruptible_capacity_reservation_allocation(
    _svc: &Ec2Service,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let id = require(&req.query_params, "CapacityReservationId")?;
    let count = require(&req.query_params, "TargetInstanceCount")?;
    let body = format!(
        "{}{}<targetInstanceCount>{}</targetInstanceCount><status>active</status><interruptionType>spot</interruptionType>",
        ec2_elem("interruptibleCapacityReservationId", &id),
        ec2_elem("sourceCapacityReservationId", &id),
        count,
    );
    Ok(Ec2Service::respond(
        "UpdateInterruptibleCapacityReservationAllocation",
        &req.request_id,
        &body,
    ))
}

#[cfg(test)]
mod crfleet_tests {
    use super::*;
    use crate::test_support::{ec2_request as req, err_of};

    fn body(resp: AwsResponse) -> String {
        String::from_utf8_lossy(resp.body.expect_bytes()).to_string()
    }

    #[test]
    fn create_capacity_reservation_persists_ebs_optimized_and_ephemeral() {
        // bug-audit 2026-07-29 (cycle 8) E2-3: CreateCapacityReservation dropped
        // EbsOptimized/EphemeralStorage; cr_xml hardcoded both false.
        let svc = Ec2Service::new();
        body(
            create_capacity_reservation(
                &svc,
                &req(
                    "CreateCapacityReservation",
                    &[
                        ("InstanceType", "t3.micro"),
                        ("InstancePlatform", "Linux/UNIX"),
                        ("AvailabilityZone", "us-east-1a"),
                        ("InstanceCount", "1"),
                        ("EbsOptimized", "true"),
                        ("EphemeralStorage", "true"),
                    ],
                ),
            )
            .unwrap(),
        );
        let desc = body(
            describe_capacity_reservations(&svc, &req("DescribeCapacityReservations", &[]))
                .unwrap(),
        );
        assert!(desc.contains("<ebsOptimized>true</ebsOptimized>"), "{desc}");
        assert!(
            desc.contains("<ephemeralStorage>true</ephemeralStorage>"),
            "{desc}"
        );
    }

    #[test]
    fn modify_capacity_reservation_persists_end_date_type_and_match() {
        // bug-audit 2026-07-28 (cycle 7) E3: ModifyCapacityReservation validated
        // EndDateType + InstanceMatchCriteria then persisted only InstanceCount,
        // so DescribeCapacityReservations read back the create-time values.
        let svc = Ec2Service::new();
        let created = body(
            create_capacity_reservation(
                &svc,
                &req(
                    "CreateCapacityReservation",
                    &[
                        ("InstanceType", "t3.micro"),
                        ("InstancePlatform", "Linux/UNIX"),
                        ("AvailabilityZone", "us-east-1a"),
                        ("InstanceCount", "4"),
                        ("InstanceMatchCriteria", "open"),
                    ],
                ),
            )
            .unwrap(),
        );
        let id = created
            .split("<capacityReservationId>")
            .nth(1)
            .unwrap()
            .split("</capacityReservationId>")
            .next()
            .unwrap()
            .to_string();
        modify_capacity_reservation(
            &svc,
            &req(
                "ModifyCapacityReservation",
                &[
                    ("CapacityReservationId", &id),
                    ("EndDateType", "unlimited"),
                    ("InstanceMatchCriteria", "targeted"),
                ],
            ),
        )
        .unwrap();
        let desc = body(
            describe_capacity_reservations(&svc, &req("DescribeCapacityReservations", &[]))
                .unwrap(),
        );
        assert!(
            desc.contains("<instanceMatchCriteria>targeted</instanceMatchCriteria>"),
            "match criteria not persisted: {desc}"
        );
        assert!(
            desc.contains("<endDateType>unlimited</endDateType>"),
            "end date type not persisted: {desc}"
        );
    }

    #[test]
    fn cr_fleet_total_target_capacity_round_trips() {
        let svc = Ec2Service::new();
        let resp = create_capacity_reservation_fleet(
            &svc,
            &req(
                "CreateCapacityReservationFleet",
                &[
                    ("TotalTargetCapacity", "10"),
                    ("Tenancy", "default"),
                    ("InstanceMatchCriteria", "open"),
                ],
            ),
        )
        .unwrap();
        let created = body(resp);
        let id = created
            .split("<capacityReservationFleetId>")
            .nth(1)
            .unwrap()
            .split("</capacityReservationFleetId>")
            .next()
            .unwrap()
            .to_string();
        let desc = body(
            describe_capacity_reservation_fleets(
                &svc,
                &req("DescribeCapacityReservationFleets", &[]),
            )
            .unwrap(),
        );
        assert!(
            desc.contains("<totalTargetCapacity>10</totalTargetCapacity>"),
            "{desc}"
        );

        modify_capacity_reservation_fleet(
            &svc,
            &req(
                "ModifyCapacityReservationFleet",
                &[
                    ("CapacityReservationFleetId", &id),
                    ("TotalTargetCapacity", "20"),
                ],
            ),
        )
        .unwrap();
        let desc2 = body(
            describe_capacity_reservation_fleets(
                &svc,
                &req("DescribeCapacityReservationFleets", &[]),
            )
            .unwrap(),
        );
        assert!(
            desc2.contains("<totalTargetCapacity>20</totalTargetCapacity>"),
            "{desc2}"
        );
    }

    #[test]
    fn modify_cr_fleet_missing_errors() {
        let svc = Ec2Service::new();
        let err = err_of(modify_capacity_reservation_fleet(
            &svc,
            &req(
                "ModifyCapacityReservationFleet",
                &[("CapacityReservationFleetId", "crf-nope")],
            ),
        ));
        assert_eq!(err.code(), "InvalidCapacityReservationFleetId.NotFound");
    }
}

#[cfg(test)]
mod date_change_quote_tests {
    use super::*;
    use crate::test_support::{ec2_request as req, err_of};

    fn body(resp: AwsResponse) -> String {
        String::from_utf8_lossy(resp.body.expect_bytes()).to_string()
    }

    fn between<'a>(s: &'a str, tag: &str) -> &'a str {
        s.split(&format!("<{tag}>"))
            .nth(1)
            .and_then(|r| r.split(&format!("</{tag}>")).next())
            .unwrap_or_else(|| panic!("<{tag}> missing in {s}"))
    }

    /// A future-dated reservation starting in `days`, with a 1-day commitment.
    fn future_cr(svc: &Ec2Service, days: i64) -> (String, DateTime<Utc>) {
        let start = Utc::now() + Duration::days(days);
        let start_s = fmt_ts(start);
        let out = body(
            create_capacity_reservation(
                svc,
                &req(
                    "CreateCapacityReservation",
                    &[
                        ("InstanceType", "m5.large"),
                        ("InstancePlatform", "Linux/UNIX"),
                        ("InstanceCount", "4"),
                        ("StartDate", &start_s),
                        ("CommitmentDuration", "86400"),
                    ],
                ),
            )
            .unwrap(),
        );
        assert_eq!(between(&out, "state"), "scheduled");
        assert_eq!(between(&out, "originalStartDate"), start_s);
        assert_eq!(between(&out, "commitmentDuration"), "86400");
        (between(&out, "capacityReservationId").to_string(), start)
    }

    fn quote(svc: &Ec2Service, cr: &str, new_start: DateTime<Utc>) -> String {
        let out = body(
            create_capacity_reservation_date_change_quote(
                svc,
                &req(
                    "CreateCapacityReservationDateChangeQuote",
                    &[
                        ("CapacityReservationId", cr),
                        ("NewStartDate", &fmt_ts(new_start)),
                        (
                            "TagSpecification.1.ResourceType",
                            "capacity-reservation-modification-quote",
                        ),
                        ("TagSpecification.1.Tag.1.Key", "team"),
                        ("TagSpecification.1.Tag.1.Value", "infra"),
                    ],
                ),
            )
            .unwrap(),
        );
        assert_eq!(between(&out, "quoteState"), "active");
        assert_eq!(between(&out, "newStartDate"), fmt_ts(new_start));
        assert_eq!(
            between(&out, "newCommitmentEndDate"),
            fmt_ts(new_start + Duration::seconds(86400))
        );
        assert_eq!(between(&out, "reservationState"), "scheduled");
        assert!(out.contains("<key>team</key>"), "{out}");
        between(&out, "capacityReservationModificationQuoteId").to_string()
    }

    #[test]
    fn quote_then_modify_pushes_out_start_date() {
        let svc = Ec2Service::new();
        let (cr, start) = future_cr(&svc, 5);
        let new_start = start + Duration::days(3);
        let qid = quote(&svc, &cr, new_start);
        assert!(qid.starts_with("crmq-"));

        let out = body(
            modify_capacity_reservation(
                &svc,
                &req(
                    "ModifyCapacityReservation",
                    &[
                        ("CapacityReservationId", &cr),
                        ("QuoteId", &qid),
                        ("StartDate", &fmt_ts(new_start)),
                        ("AcceptModificationTerms", "true"),
                    ],
                ),
            )
            .unwrap(),
        );
        assert_eq!(between(&out, "adjustmentStatus"), "applied");
        assert_eq!(between(&out, "startDate"), fmt_ts(new_start));
        assert_eq!(
            between(&out, "commitmentEndDate"),
            fmt_ts(new_start + Duration::seconds(86400))
        );

        let desc = body(
            describe_capacity_reservations(&svc, &req("DescribeCapacityReservations", &[]))
                .unwrap(),
        );
        assert_eq!(between(&desc, "startDate"), fmt_ts(new_start));
        // The original start date is the anchor; it doesn't move.
        assert_eq!(between(&desc, "originalStartDate"), fmt_ts(start));
        assert_eq!(between(&desc, "adjustmentStatus"), "applied");

        // A quote is single-use: it now reports expired and can't be reapplied.
        let quotes = body(
            describe_capacity_reservation_date_change_quotes(
                &svc,
                &req(
                    "DescribeCapacityReservationDateChangeQuotes",
                    &[("CapacityReservationModificationQuoteId.1", &qid)],
                ),
            )
            .unwrap(),
        );
        assert_eq!(between(&quotes, "quoteState"), "expired");
        let err = err_of(modify_capacity_reservation(
            &svc,
            &req(
                "ModifyCapacityReservation",
                &[
                    ("CapacityReservationId", &cr),
                    ("QuoteId", &qid),
                    ("AcceptModificationTerms", "true"),
                ],
            ),
        ));
        assert_eq!(err.code(), "IncorrectState");
    }

    #[test]
    fn applying_a_quote_spends_the_other_quotes_for_the_reservation() {
        let svc = Ec2Service::new();
        let (cr, start) = future_cr(&svc, 5);
        let early = quote(&svc, &cr, start + Duration::days(1));
        let late_start = start + Duration::days(3);
        let late = quote(&svc, &cr, late_start);
        // StartDate at microsecond precision (as boto3 sends it) still
        // matches the millisecond-precision quoted date.
        let micro = format!("{}Z", late_start.format("%Y-%m-%dT%H:%M:%S%.6f"));
        modify_capacity_reservation(
            &svc,
            &req(
                "ModifyCapacityReservation",
                &[
                    ("CapacityReservationId", &cr),
                    ("QuoteId", &late),
                    ("StartDate", &micro),
                    ("AcceptModificationTerms", "true"),
                ],
            ),
        )
        .unwrap();
        // The earlier quote can no longer pull the start date back.
        let err = err_of(modify_capacity_reservation(
            &svc,
            &req(
                "ModifyCapacityReservation",
                &[
                    ("CapacityReservationId", &cr),
                    ("QuoteId", &early),
                    ("AcceptModificationTerms", "true"),
                ],
            ),
        ));
        assert_eq!(err.code(), "IncorrectState");
        let desc = body(
            describe_capacity_reservations(&svc, &req("DescribeCapacityReservations", &[]))
                .unwrap(),
        );
        assert_eq!(between(&desc, "startDate"), fmt_ts(late_start));
    }

    #[test]
    fn quote_client_token_reuse_with_a_different_date_is_a_mismatch() {
        let svc = Ec2Service::new();
        let (cr, start) = future_cr(&svc, 5);
        let call = |days: i64| {
            create_capacity_reservation_date_change_quote(
                &svc,
                &req(
                    "CreateCapacityReservationDateChangeQuote",
                    &[
                        ("CapacityReservationId", &cr),
                        ("NewStartDate", &fmt_ts(start + Duration::days(days))),
                        ("ClientToken", "tok-1"),
                    ],
                ),
            )
        };
        let first = between(
            &body(call(1).unwrap()),
            "capacityReservationModificationQuoteId",
        )
        .to_string();
        let again = between(
            &body(call(1).unwrap()),
            "capacityReservationModificationQuoteId",
        )
        .to_string();
        assert_eq!(first, again);
        assert_eq!(err_of(call(2)).code(), "IdempotentParameterMismatch");
    }

    #[test]
    fn switching_to_unlimited_drops_the_end_date() {
        let svc = Ec2Service::new();
        let (cr, start) = future_cr(&svc, 5);
        let end = fmt_ts(start + Duration::days(30));
        let out = body(
            modify_capacity_reservation(
                &svc,
                &req(
                    "ModifyCapacityReservation",
                    &[
                        ("CapacityReservationId", &cr),
                        ("EndDateType", "limited"),
                        ("EndDate", &end),
                    ],
                ),
            )
            .unwrap(),
        );
        assert!(out.contains(&format!("<endDate>{end}</endDate>")), "{out}");
        modify_capacity_reservation(
            &svc,
            &req(
                "ModifyCapacityReservation",
                &[("CapacityReservationId", &cr), ("EndDateType", "unlimited")],
            ),
        )
        .unwrap();
        let desc = body(
            describe_capacity_reservations(&svc, &req("DescribeCapacityReservations", &[]))
                .unwrap(),
        );
        assert!(!desc.contains("<endDate>"), "{desc}");
        assert_eq!(between(&desc, "endDateType"), "unlimited");
    }

    #[test]
    fn quote_enforces_pushout_window_and_future_dating() {
        let svc = Ec2Service::new();
        let (cr, start) = future_cr(&svc, 5);
        let call = |new_start: DateTime<Utc>, id: &str| {
            err_of(create_capacity_reservation_date_change_quote(
                &svc,
                &req(
                    "CreateCapacityReservationDateChangeQuote",
                    &[
                        ("CapacityReservationId", id),
                        ("NewStartDate", &fmt_ts(new_start)),
                    ],
                ),
            ))
            .code()
            .to_string()
        };
        assert_eq!(
            call(start - Duration::hours(1), &cr),
            "InvalidParameterValue"
        );
        assert_eq!(
            call(start + Duration::days(31), &cr),
            "InvalidParameterValue"
        );
        assert_eq!(
            call(start, "cr-00000000000000000"),
            "InvalidCapacityReservationId.NotFound"
        );

        // An immediate (already delivered) reservation has no date to change.
        let now_cr = body(
            create_capacity_reservation(
                &svc,
                &req(
                    "CreateCapacityReservation",
                    &[
                        ("InstanceType", "m5.large"),
                        ("InstancePlatform", "Linux/UNIX"),
                        ("InstanceCount", "1"),
                    ],
                ),
            )
            .unwrap(),
        );
        let now_id = between(&now_cr, "capacityReservationId").to_string();
        assert_eq!(
            call(Utc::now() + Duration::days(1), &now_id),
            "IncorrectState"
        );
    }

    #[test]
    fn modify_requires_accepting_quote_terms() {
        let svc = Ec2Service::new();
        let (cr, start) = future_cr(&svc, 5);
        let qid = quote(&svc, &cr, start + Duration::days(1));
        let err = err_of(modify_capacity_reservation(
            &svc,
            &req(
                "ModifyCapacityReservation",
                &[("CapacityReservationId", &cr), ("QuoteId", &qid)],
            ),
        ));
        assert_eq!(err.code(), "InvalidParameterValue");
        let err = err_of(modify_capacity_reservation(
            &svc,
            &req(
                "ModifyCapacityReservation",
                &[
                    ("CapacityReservationId", &cr),
                    ("StartDate", &fmt_ts(start + Duration::days(1))),
                ],
            ),
        ));
        assert_eq!(err.code(), "MissingParameter");
        let err = err_of(modify_capacity_reservation(
            &svc,
            &req(
                "ModifyCapacityReservation",
                &[
                    ("CapacityReservationId", &cr),
                    ("QuoteId", &qid),
                    ("StartDate", &fmt_ts(start + Duration::days(2))),
                    ("AcceptModificationTerms", "true"),
                ],
            ),
        ));
        assert_eq!(err.code(), "InvalidParameterValue");
    }

    #[test]
    fn describe_quotes_filters_paginates_and_rejects_unknown_ids() {
        let svc = Ec2Service::new();
        let (cr_a, start_a) = future_cr(&svc, 5);
        let (cr_b, start_b) = future_cr(&svc, 6);
        quote(&svc, &cr_a, start_a + Duration::days(1));
        quote(&svc, &cr_a, start_a + Duration::days(2));
        let qb = quote(&svc, &cr_b, start_b + Duration::days(1));

        let describe = |q: &[(&str, &str)]| {
            body(
                describe_capacity_reservation_date_change_quotes(
                    &svc,
                    &req("DescribeCapacityReservationDateChangeQuotes", q),
                )
                .unwrap(),
            )
        };
        let only_b = describe(&[
            ("Filter.1.Name", "capacity-reservation-id"),
            ("Filter.1.Value.1", &cr_b),
        ]);
        assert_eq!(
            only_b
                .matches("<capacityReservationModificationQuoteId>")
                .count(),
            1,
            "{only_b}"
        );
        assert!(only_b.contains(&qb));
        let tagged = describe(&[("Filter.1.Name", "tag:team"), ("Filter.1.Value.1", "infra")]);
        assert_eq!(
            tagged
                .matches("<capacityReservationModificationQuoteId>")
                .count(),
            3
        );

        let page1 = describe(&[("MaxResults", "2")]);
        assert_eq!(
            page1
                .matches("<capacityReservationModificationQuoteId>")
                .count(),
            2
        );
        let token = between(&page1, "nextToken").to_string();
        let page2 = describe(&[("MaxResults", "2"), ("NextToken", &token)]);
        assert_eq!(
            page2
                .matches("<capacityReservationModificationQuoteId>")
                .count(),
            1
        );
        assert!(!page2.contains("<nextToken>"));

        let err = err_of(describe_capacity_reservation_date_change_quotes(
            &svc,
            &req(
                "DescribeCapacityReservationDateChangeQuotes",
                &[(
                    "CapacityReservationModificationQuoteId.1",
                    "crmq-00000000000000000",
                )],
            ),
        ));
        assert_eq!(
            err.code(),
            "InvalidCapacityReservationModificationQuoteId.NotFound"
        );
    }
}
