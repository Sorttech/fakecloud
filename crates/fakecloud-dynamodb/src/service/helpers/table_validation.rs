//! Request validation for the table control plane (CreateTable, UpdateTable).
//!
//! DynamoDB validates a request in two layers. The request-model layer checks
//! each member against its modeled constraints (lengths, patterns, ranges,
//! enums) and reports every violation at once as
//! `N validation error(s) detected: ...; ...`. Only a request that passes it
//! reaches the service layer, whose rejections are single, bare
//! `One or more parameter values were invalid: ...` messages. This module
//! reproduces both, in that order.

use super::*;

/// Maximum number of vector indexes a table may carry.
pub(crate) const MAX_VECTOR_INDEXES_PER_TABLE: usize = 5;
/// Largest `Dimensions` a vector index accepts.
pub(crate) const MAX_VECTOR_DIMENSIONS: i64 = 4096;

const TABLE_NAME_PATTERN: &str = "[a-zA-Z0-9_.-]+";

/// Collected request-model violations, rendered the way DynamoDB renders them.
///
/// A member of the wrong JSON type never reaches validation: the request
/// fails to deserialize, and that `SerializationException` is the answer.
#[derive(Default)]
pub(crate) struct ModelErrors {
    errors: Vec<String>,
    serialization: Option<String>,
}

/// How a request deserializer reports a JSON value of the wrong type for a
/// member of type `target` (`String`, `Long`, ...).
fn wrong_type_message(value: &Value, target: &str) -> String {
    match value {
        Value::Array(_) => "Start of list found where not expected".to_string(),
        Value::Object(_) => "Start of structure or map found where not expected.".to_string(),
        Value::Bool(true) => format!("TRUE_VALUE can not be converted to a {target}"),
        Value::Bool(false) => format!("FALSE_VALUE can not be converted to a {target}"),
        Value::Number(_) => format!("NUMBER_VALUE can not be converted to a {target}"),
        _ => format!("STRING_VALUE can not be converted to a {target}"),
    }
}

impl ModelErrors {
    pub(crate) fn push(&mut self, message: String) {
        self.errors.push(message);
    }

    pub(crate) fn into_result(self) -> Result<(), AwsServiceError> {
        if let Some(message) = self.serialization {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "SerializationException",
                message,
            ));
        }
        if self.errors.is_empty() {
            return Ok(());
        }
        let n = self.errors.len();
        let noun = if n == 1 { "error" } else { "errors" };
        Err(AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            format!("{n} validation {noun} detected: {}", self.errors.join("; ")),
        ))
    }

    fn wrong_type(&mut self, value: &Value, target: &str) {
        if self.serialization.is_none() {
            self.serialization = Some(wrong_type_message(value, target));
        }
    }

    /// A string member: `None` when absent, and when of the wrong type (which
    /// is recorded as the deserialization failure it is).
    fn string<'v>(&mut self, value: &'v Value) -> Option<&'v str> {
        match value {
            Value::Null => None,
            Value::String(s) => Some(s),
            other => {
                self.wrong_type(other, "String");
                None
            }
        }
    }

    /// A list member: `None` when absent or of the wrong type.
    fn list<'v>(&mut self, value: &'v Value) -> Option<&'v Vec<Value>> {
        match value {
            Value::Null => None,
            Value::Array(a) => Some(a),
            other => {
                self.wrong_type(other, "List");
                None
            }
        }
    }

    fn not_null(&mut self, path: &str) {
        self.push(format!(
            "Value null at '{path}' failed to satisfy constraint: Member must not be null"
        ));
    }

    fn check_enum(&mut self, path: &str, value: &Value, allowed: &[&str]) {
        if let Some(s) = self.string(value) {
            if !allowed.contains(&s) {
                self.push(format!(
                    "Value '{s}' at '{path}' failed to satisfy constraint: \
                     Member must satisfy enum value set: [{}]",
                    allowed.join(", ")
                ));
            }
        }
    }

    fn check_length(&mut self, path: &str, value: &str, min: usize, max: usize) {
        let len = value.chars().count();
        if len < min {
            self.push(format!(
                "Value '{value}' at '{path}' failed to satisfy constraint: \
                 Member must have length greater than or equal to {min}"
            ));
        }
        if len > max {
            self.push(format!(
                "Value '{value}' at '{path}' failed to satisfy constraint: \
                 Member must have length less than or equal to {max}"
            ));
        }
    }

    fn check_min(&mut self, path: &str, value: &Value, min: i64) {
        if !value.is_null() && value.as_i64().is_none() {
            self.wrong_type(value, "Long");
        }
        if let Some(n) = value.as_i64() {
            if n < min {
                self.push(format!(
                    "Value '{n}' at '{path}' failed to satisfy constraint: \
                     Member must have value greater than or equal to {min}"
                ));
            }
        }
    }

    /// An index or table name: 3..=255 characters of `[a-zA-Z0-9_.-]`.
    fn check_name(&mut self, path: &str, value: &str) {
        self.check_length(path, value, 3, 255);
        if !is_valid_name(value) {
            self.push(format!(
                "Value '{value}' at '{path}' failed to satisfy constraint: \
                 Member must satisfy regular expression pattern: {TABLE_NAME_PATTERN}"
            ));
        }
    }

    fn check_key_schema(&mut self, path: &str, value: &Value) {
        let Some(arr) = self.list(value) else {
            if value.is_null() {
                self.not_null(path);
            }
            return;
        };
        if arr.is_empty() {
            self.push(format!(
                "Value '[]' at '{path}' failed to satisfy constraint: \
                 Member must have length greater than or equal to 1"
            ));
        }
        if arr.len() > 2 {
            let rendered: Vec<String> = arr
                .iter()
                .map(|e| {
                    format!(
                        "KeySchemaElement(attributeName={}, keyType={})",
                        e["AttributeName"].as_str().unwrap_or("null"),
                        e["KeyType"].as_str().unwrap_or("null")
                    )
                })
                .collect();
            self.push(format!(
                "Value '[{}]' at '{path}' failed to satisfy constraint: \
                 Member must have length less than or equal to 2",
                rendered.join(", ")
            ));
        }
        for (i, elem) in arr.iter().enumerate() {
            let member = format!("{path}.{}.member", i + 1);
            match self.string(&elem["AttributeName"]) {
                Some(name) => self.check_length(&format!("{member}.attributeName"), name, 1, 255),
                None => self.not_null(&format!("{member}.attributeName")),
            }
            if elem["KeyType"].is_null() {
                self.not_null(&format!("{member}.keyType"));
            } else {
                self.check_enum(
                    &format!("{member}.keyType"),
                    &elem["KeyType"],
                    &["HASH", "RANGE"],
                );
            }
        }
    }

    fn check_projection(&mut self, path: &str, value: &Value) {
        if value.is_null() {
            self.not_null(path);
            return;
        }
        self.check_enum(
            &format!("{path}.projectionType"),
            &value["ProjectionType"],
            &["ALL", "KEYS_ONLY", "INCLUDE"],
        );
        if let Some(arr) = value["NonKeyAttributes"].as_array() {
            if arr.len() > 20 {
                self.push(format!(
                    "Value at '{path}.nonKeyAttributes' failed to satisfy constraint: \
                     Member must have length less than or equal to 20"
                ));
            }
        }
    }

    fn check_throughput(&mut self, path: &str, value: &Value) {
        if !value.is_object() {
            return;
        }
        for (field, member) in [
            ("ReadCapacityUnits", "readCapacityUnits"),
            ("WriteCapacityUnits", "writeCapacityUnits"),
        ] {
            let v = &value[field];
            if v.is_null() {
                self.not_null(&format!("{path}.{member}"));
            } else {
                self.check_min(&format!("{path}.{member}"), v, 1);
            }
        }
    }

    fn check_attribute_definitions(&mut self, value: &Value, required: bool) {
        let Some(defs) = self.list(value) else {
            if required && value.is_null() {
                self.not_null("attributeDefinitions");
            }
            return;
        };
        for (i, def) in defs.iter().enumerate() {
            let member = format!("attributeDefinitions.{}.member", i + 1);
            match self.string(&def["AttributeName"]) {
                Some(name) => self.check_length(&format!("{member}.attributeName"), name, 1, 255),
                None => self.not_null(&format!("{member}.attributeName")),
            }
            if def["AttributeType"].is_null() {
                self.not_null(&format!("{member}.attributeType"));
            } else {
                self.check_enum(
                    &format!("{member}.attributeType"),
                    &def["AttributeType"],
                    &["B", "N", "S"],
                );
            }
        }
    }

    fn check_vector_index(&mut self, path: &str, v: &Value) {
        match self.string(&v["IndexName"]) {
            Some(name) => self.check_name(&format!("{path}.indexName"), name),
            None => self.not_null(&format!("{path}.indexName")),
        }
        match self.string(&v["VectorAttribute"]["AttributeName"]) {
            Some(name) => self.check_length(
                &format!("{path}.vectorAttribute.attributeName"),
                name,
                1,
                255,
            ),
            None => self.not_null(&format!("{path}.vectorAttribute")),
        }
        if v["Dimensions"].is_null() {
            self.not_null(&format!("{path}.dimensions"));
        } else {
            self.check_min(&format!("{path}.dimensions"), &v["Dimensions"], 1);
        }
        if v["DistanceFunction"].is_null() {
            self.not_null(&format!("{path}.distanceFunction"));
        } else {
            self.check_enum(
                &format!("{path}.distanceFunction"),
                &v["DistanceFunction"],
                VECTOR_DISTANCE_FUNCTIONS,
            );
        }
        if let Some(schema) = self.list(&v["SearchSchema"]) {
            if schema.is_empty() {
                self.push(format!(
                    "Value '[]' at '{path}.searchSchema' failed to satisfy constraint: \
                     Member must have length greater than or equal to 1"
                ));
            }
            for (i, e) in schema.iter().enumerate() {
                let member = format!("{path}.searchSchema.{}.member", i + 1);
                match self.string(&e["AttributeName"]) {
                    Some(name) => {
                        self.check_length(&format!("{member}.attributeName"), name, 0, 65535)
                    }
                    None => self.not_null(&format!("{member}.attributeName")),
                }
                if e["SearchSchemaElementType"].is_null() {
                    self.not_null(&format!("{member}.searchSchemaElementType"));
                } else {
                    self.check_enum(
                        &format!("{member}.searchSchemaElementType"),
                        &e["SearchSchemaElementType"],
                        &["HASH", "INLINE_FILTER"],
                    );
                }
            }
        }
        self.check_projection(&format!("{path}.projection"), &v["Projection"]);
    }
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn invalid(message: impl std::fmt::Display) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "ValidationException",
        format!("One or more parameter values were invalid: {message}"),
    )
}

/// The request-model layer of CreateTable.
///
/// A missing `TableName` is reported on its own, as is any violation of the
/// table name's constraints: those are resolved before the rest of the request
/// is looked at. Everything else is collected and reported together.
pub(crate) fn validate_create_table_model(body: &Value) -> Result<(), AwsServiceError> {
    let mut errors = ModelErrors::default();
    errors.check_attribute_definitions(&body["AttributeDefinitions"], true);
    errors.check_key_schema("keySchema", &body["KeySchema"]);
    for (field, path) in [
        ("LocalSecondaryIndexes", "localSecondaryIndexes"),
        ("GlobalSecondaryIndexes", "globalSecondaryIndexes"),
    ] {
        for (i, idx) in errors.list(&body[field]).into_iter().flatten().enumerate() {
            let member = format!("{path}.{}.member", i + 1);
            match errors.string(&idx["IndexName"]) {
                Some(name) => errors.check_name(&format!("{member}.indexName"), name),
                None => errors.not_null(&format!("{member}.indexName")),
            }
            errors.check_key_schema(&format!("{member}.keySchema"), &idx["KeySchema"]);
            errors.check_projection(&format!("{member}.projection"), &idx["Projection"]);
            errors.check_throughput(
                &format!("{member}.provisionedThroughput"),
                &idx["ProvisionedThroughput"],
            );
        }
    }
    errors.check_enum(
        "billingMode",
        &body["BillingMode"],
        &["PROVISIONED", "PAY_PER_REQUEST"],
    );
    errors.check_throughput("provisionedThroughput", &body["ProvisionedThroughput"]);
    if body["StreamSpecification"].is_object() {
        if body["StreamSpecification"]["StreamEnabled"].is_null() {
            errors.not_null("streamSpecification.streamEnabled");
        }
        errors.check_enum(
            "streamSpecification.streamViewType",
            &body["StreamSpecification"]["StreamViewType"],
            &["NEW_IMAGE", "OLD_IMAGE", "NEW_AND_OLD_IMAGES", "KEYS_ONLY"],
        );
    }
    errors.check_enum(
        "sSESpecification.sSEType",
        &body["SSESpecification"]["SSEType"],
        &["AES256", "KMS"],
    );
    errors.check_enum(
        "tableClass",
        &body["TableClass"],
        &["STANDARD", "STANDARD_INFREQUENT_ACCESS"],
    );
    for (i, v) in errors
        .list(&body["VectorIndexes"])
        .into_iter()
        .flatten()
        .enumerate()
    {
        errors.check_vector_index(&format!("vectorIndexes.{}.member", i + 1), v);
    }
    // A member of the wrong type fails deserialization, before any
    // validation -- including the table name's.
    if let Some(message) = errors.serialization.take() {
        return Err(AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "SerializationException",
            message,
        ));
    }
    if !body["TableName"].is_null() && !body["TableName"].is_string() {
        return Err(AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "SerializationException",
            wrong_type_message(&body["TableName"], "String"),
        ));
    }
    let Some(table_name) = body["TableName"].as_str() else {
        return Err(AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            "The parameter 'TableName' is required but was not present in the request",
        ));
    };
    let mut name_errors = ModelErrors::default();
    name_errors.check_name("tableName", table_name);
    name_errors.into_result()?;

    errors.into_result()
}

/// The request-model layer of UpdateTable.
pub(crate) fn validate_update_table_model(body: &Value) -> Result<(), AwsServiceError> {
    let mut errors = ModelErrors::default();
    if let Some(name) = errors.string(&body["TableName"]) {
        // UpdateTable also takes a table ARN, so only the length is modeled.
        errors.check_length("tableName", name, 1, 1024);
    }
    errors.check_attribute_definitions(&body["AttributeDefinitions"], false);
    errors.check_enum(
        "billingMode",
        &body["BillingMode"],
        &["PROVISIONED", "PAY_PER_REQUEST"],
    );
    errors.check_throughput("provisionedThroughput", &body["ProvisionedThroughput"]);
    for (i, update) in errors
        .list(&body["GlobalSecondaryIndexUpdates"])
        .into_iter()
        .flatten()
        .enumerate()
    {
        let member = format!("globalSecondaryIndexUpdates.{}.member", i + 1);
        if let Some(create) = update.get("Create").filter(|v| v.is_object()) {
            let path = format!("{member}.create");
            match errors.string(&create["IndexName"]) {
                Some(name) => errors.check_name(&format!("{path}.indexName"), name),
                None => errors.not_null(&format!("{path}.indexName")),
            }
            errors.check_key_schema(&format!("{path}.keySchema"), &create["KeySchema"]);
            errors.check_projection(&format!("{path}.projection"), &create["Projection"]);
            errors.check_throughput(
                &format!("{path}.provisionedThroughput"),
                &create["ProvisionedThroughput"],
            );
        }
        if let Some(update) = update.get("Update").filter(|v| v.is_object()) {
            errors.check_throughput(
                &format!("{member}.update.provisionedThroughput"),
                &update["ProvisionedThroughput"],
            );
        }
    }
    for (i, update) in errors
        .list(&body["VectorIndexUpdates"])
        .into_iter()
        .flatten()
        .enumerate()
    {
        let member = format!("vectorIndexUpdates.{}.member", i + 1);
        if let Some(create) = update.get("Create").filter(|v| v.is_object()) {
            errors.check_vector_index(&format!("{member}.create"), create);
        }
        if let Some(delete) = update.get("Delete").filter(|v| v.is_object()) {
            match errors.string(&delete["IndexName"]) {
                Some(name) => errors.check_name(&format!("{member}.delete.indexName"), name),
                None => errors.not_null(&format!("{member}.delete.indexName")),
            }
        }
    }
    errors.check_enum(
        "tableClass",
        &body["TableClass"],
        &["STANDARD", "STANDARD_INFREQUENT_ACCESS"],
    );
    errors.into_result()
}

fn online_index_limit() -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "LimitExceededException",
        "Subscriber limit exceeded: Only 1 online index can be created or deleted \
         simultaneously per table",
    )
}

fn index_not_found(name: &str) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "ResourceNotFoundException",
        format!("Requested resource not found: Index: {name} not found"),
    )
}

/// The service-layer checks of UpdateTable against the table as it stands.
/// Runs before anything is changed, so a rejected request changes nothing.
pub(crate) fn validate_update_table_request(
    table: &DynamoTable,
    body: &Value,
) -> Result<(), AwsServiceError> {
    let target_billing = body["BillingMode"]
        .as_str()
        .unwrap_or(table.billing_mode.as_str());
    let now = chrono::Utc::now();
    // A vector index still being built holds the table's one online index
    // action; while it allocates resources, the table itself is UPDATING.
    let building = table
        .vector_indexes
        .iter()
        .any(|v| v.phase(now) != VectorIndexPhase::Active);
    let allocating = table
        .vector_indexes
        .iter()
        .any(|v| v.phase(now) == VectorIndexPhase::Allocating);

    // Vector indexes require on-demand billing for as long as the table has one.
    if target_billing != "PAY_PER_REQUEST" && !table.vector_indexes.is_empty() {
        return Err(invalid(
            "Vector indexes are only supported for PAY_PER_REQUEST tables",
        ));
    }

    // A throughput "change" to the values already provisioned is refused.
    if table.billing_mode == "PROVISIONED" && target_billing == "PROVISIONED" {
        if let Some(pt) = body["ProvisionedThroughput"].as_object() {
            let current = &table.provisioned_throughput;
            let read = pt.get("ReadCapacityUnits").and_then(Value::as_i64);
            let write = pt.get("WriteCapacityUnits").and_then(Value::as_i64);
            if read == Some(current.read_capacity_units)
                && write == Some(current.write_capacity_units)
            {
                return Err(AwsServiceError::aws_error(
                    StatusCode::BAD_REQUEST,
                    "ValidationException",
                    format!(
                        "The provisioned throughput for the table will not change. The requested \
                         value equals the current value. Current ReadCapacityUnits provisioned \
                         for the table: {r}. Requested ReadCapacityUnits: {r}. Current \
                         WriteCapacityUnits provisioned for the table: {w}. Requested \
                         WriteCapacityUnits: {w}. Refer to the Amazon DynamoDB Developer Guide \
                         for current limits and how to request higher limits.",
                        r = current.read_capacity_units,
                        w = current.write_capacity_units,
                    ),
                ));
            }
        }
    }

    let gsi_updates = body["GlobalSecondaryIndexUpdates"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let vector_updates = body["VectorIndexUpdates"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    // One online index action (create or delete) per request.
    let online_actions = gsi_updates
        .iter()
        .chain(vector_updates)
        .filter(|u| u.get("Create").is_some() || u.get("Delete").is_some())
        .count();
    if online_actions > 1 {
        return Err(online_index_limit());
    }

    let request_defs = body["AttributeDefinitions"]
        .as_array()
        .map(|_| parse_attribute_definitions(&body["AttributeDefinitions"]))
        .transpose()?
        .unwrap_or_default();
    let index_exists = |name: &str| {
        table.gsi.iter().any(|g| g.index_name == name)
            || table.lsi.iter().any(|l| l.index_name == name)
            || table.vector_indexes.iter().any(|v| v.index_name == name)
    };
    let already_exists = || {
        AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            "Attempting to create an index which already exists",
        )
    };

    for update in gsi_updates {
        if building && (update.get("Create").is_some() || update.get("Delete").is_some()) {
            return Err(online_index_limit());
        }
        if let Some(create) = update.get("Create") {
            let name = create["IndexName"].as_str().unwrap_or_default();
            if index_exists(name) {
                return Err(already_exists());
            }
            // The new index's keys must be defined in this request's own
            // AttributeDefinitions; the table's stored definitions do not count.
            let key_schema = parse_key_schema(&create["KeySchema"])?;
            let missing: Vec<&str> = key_schema
                .iter()
                .map(|k| k.attribute_name.as_str())
                .filter(|a| !request_defs.iter().any(|d| d.attribute_name == *a))
                .collect();
            if !missing.is_empty() {
                return Err(invalid(format!(
                    "Some index key attributes are not defined in AttributeDefinitions. \
                     Keys: [{}], AttributeDefinitions: [{}]",
                    missing.join(", "),
                    request_defs
                        .iter()
                        .map(|d| d.attribute_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            validate_projection(&create["Projection"])?;
        }
        for action in ["Update", "Delete"] {
            if let Some(name) = update[action]["IndexName"].as_str() {
                if !table.gsi.iter().any(|g| g.index_name == name) {
                    return Err(index_not_found(name));
                }
            }
        }
    }

    for update in vector_updates {
        if let Some(create) = update.get("Create") {
            let name = create["IndexName"].as_str().unwrap_or_default();
            if building {
                return Err(online_index_limit());
            }
            if index_exists(name) {
                return Err(already_exists());
            }
            let defs: Vec<AttributeDefinition> = table
                .attribute_definitions
                .iter()
                .chain(&request_defs)
                .cloned()
                .collect();
            let others: std::collections::HashSet<&str> = table
                .gsi
                .iter()
                .map(|g| g.index_name.as_str())
                .chain(table.lsi.iter().map(|l| l.index_name.as_str()))
                .collect();
            validate_vector_index_definitions(
                std::slice::from_ref(create),
                target_billing,
                &defs,
                &table.vector_indexes,
                &others,
            )?;
        }
        if let Some(name) = update["Delete"]["IndexName"].as_str() {
            let index = table
                .vector_indexes
                .iter()
                .find(|v| v.index_name == name)
                .ok_or_else(|| index_not_found(name))?;
            if index.phase(now) == VectorIndexPhase::Allocating {
                return Err(AwsServiceError::aws_error(
                    StatusCode::BAD_REQUEST,
                    "ResourceInUseException",
                    format!(
                        "Attempt to change a resource which is still in use: Index creation is \
                         in resource allocation phase. Retry deletion during backfilling phase \
                         or when the index is active. Table: {} Index: {name}",
                        table.name
                    ),
                ));
            }
        }
    }
    // Any other change waits until the table is ACTIVE again.
    if allocating {
        return Err(AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ResourceInUseException",
            format!(
                "Attempt to change a resource which is still in use: Table is being updated: {}",
                table.name
            ),
        ));
    }
    Ok(())
}

/// Validate a secondary index's `Projection` at the service layer.
pub(crate) fn validate_projection(projection: &Value) -> Result<(), AwsServiceError> {
    let projection_type = projection["ProjectionType"].as_str().unwrap_or("ALL");
    let has_non_key = projection["NonKeyAttributes"]
        .as_array()
        .is_some_and(|a| !a.is_empty());
    match (projection_type, has_non_key) {
        ("INCLUDE", false) => Err(invalid(
            "ProjectionType is INCLUDE, but NonKeyAttributes is not specified",
        )),
        ("ALL" | "KEYS_ONLY", true) => Err(invalid(format!(
            "ProjectionType is {projection_type}, but NonKeyAttributes is specified"
        ))),
        _ => Ok(()),
    }
}

/// Reject `ProvisionedThroughput` alongside `BillingMode: PAY_PER_REQUEST`.
pub(crate) fn validate_no_throughput_for_on_demand(body: &Value) -> Result<(), AwsServiceError> {
    if body["BillingMode"].as_str() == Some("PAY_PER_REQUEST")
        && body["ProvisionedThroughput"].is_object()
    {
        return Err(invalid(
            "Neither ReadCapacityUnits nor WriteCapacityUnits can be specified when \
             BillingMode is PAY_PER_REQUEST",
        ));
    }
    Ok(())
}

/// The service-layer checks of CreateTable that need only the request.
///
/// Runs after [`validate_create_table_model`], and after `KeySchema` and
/// `AttributeDefinitions` have been parsed (so their structure is known).
pub(crate) fn validate_create_table_semantics(
    body: &Value,
    key_schema: &[KeySchemaElement],
    attribute_definitions: &[AttributeDefinition],
) -> Result<(), AwsServiceError> {
    validate_no_throughput_for_on_demand(body)?;
    let billing_mode = body["BillingMode"].as_str().unwrap_or("PROVISIONED");
    if billing_mode == "PROVISIONED" && !body["ProvisionedThroughput"].is_object() {
        return Err(invalid(
            "ReadCapacityUnits and WriteCapacityUnits must both be specified when \
             BillingMode is PROVISIONED",
        ));
    }
    if let Some(spec) = body["StreamSpecification"].as_object() {
        let enabled = spec
            .get("StreamEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let has_view_type = spec.get("StreamViewType").is_some_and(|v| !v.is_null());
        if !enabled && has_view_type {
            return Err(invalid(
                "Table is being created with a stream disabled, UpdateViewType should not be \
                 specified",
            ));
        }
    }

    // A key attribute named twice leaves one of the two key elements without
    // a definition of its own.
    if key_schema.len() == 2 && key_schema[0].attribute_name == key_schema[1].attribute_name {
        return Err(AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            "Invalid KeySchema: Some index key attribute have no definition",
        ));
    }

    let table_hash = key_schema
        .iter()
        .find(|k| k.key_type == "HASH")
        .map(|k| k.attribute_name.as_str())
        .unwrap_or_default();
    let table_has_range = key_schema.iter().any(|k| k.key_type == "RANGE");

    let lsis = body["LocalSecondaryIndexes"].as_array();
    if lsis.is_some_and(|l| !l.is_empty()) && !table_has_range {
        return Err(invalid(
            "Table KeySchema does not have a range key, which is required when specifying a \
             LocalSecondaryIndex",
        ));
    }
    for lsi in lsis.into_iter().flatten() {
        let name = lsi["IndexName"].as_str().unwrap_or_default();
        let ks = lsi["KeySchema"].as_array().cloned().unwrap_or_default();
        let hash = ks
            .iter()
            .find(|k| k["KeyType"] == "HASH")
            .and_then(|k| k["AttributeName"].as_str())
            .unwrap_or_default();
        if hash != table_hash {
            return Err(invalid(format!(
                "Index KeySchema does not have the same leading hash key as table KeySchema for \
                 index: {name}. index hash key: {hash}, table hash key: {table_hash}"
            )));
        }
        if !ks.iter().any(|k| k["KeyType"] == "RANGE") {
            return Err(invalid(format!(
                "Index KeySchema does not have a range key for index: {name}"
            )));
        }
    }

    let mut seen = std::collections::HashSet::new();
    for idx in lsis.into_iter().flatten().chain(
        body["GlobalSecondaryIndexes"]
            .as_array()
            .into_iter()
            .flatten(),
    ) {
        let name = idx["IndexName"].as_str().unwrap_or_default();
        if !seen.insert(name) {
            return Err(invalid(format!("Duplicate index name: {name}")));
        }
        validate_projection(&idx["Projection"])?;
    }

    validate_vector_index_definitions(
        body["VectorIndexes"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        billing_mode,
        attribute_definitions,
        &[],
        &seen,
    )?;

    Ok(())
}

/// Every `AttributeDefinitions` entry must back a key of the table, of one of
/// its secondary indexes, or an element of a vector index's `SearchSchema`.
pub(crate) fn validate_attribute_definitions_used(
    body: &Value,
    key_schema: &[KeySchemaElement],
    attribute_definitions: &[AttributeDefinition],
) -> Result<(), AwsServiceError> {
    let lsis = body["LocalSecondaryIndexes"].as_array();
    let used: std::collections::HashSet<&str> = key_schema
        .iter()
        .map(|k| k.attribute_name.as_str())
        .chain(
            lsis.into_iter()
                .flatten()
                .chain(
                    body["GlobalSecondaryIndexes"]
                        .as_array()
                        .into_iter()
                        .flatten(),
                )
                .flat_map(|idx| idx["KeySchema"].as_array().into_iter().flatten())
                .filter_map(|k| k["AttributeName"].as_str()),
        )
        .chain(
            body["VectorIndexes"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|v| v["SearchSchema"].as_array().into_iter().flatten())
                .filter_map(|e| e["AttributeName"].as_str()),
        )
        .collect();
    if attribute_definitions
        .iter()
        .any(|ad| !used.contains(ad.attribute_name.as_str()))
    {
        let mut keys_used: Vec<&str> = used.into_iter().collect();
        keys_used.sort_unstable();
        return Err(invalid(format!(
            "Some AttributeDefinitions are not used. AttributeDefinitions: [{}], keys used: [{}]",
            attribute_definitions
                .iter()
                .map(|ad| ad.attribute_name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            keys_used.join(", ")
        )));
    }
    Ok(())
}

/// Service-layer checks on vector index definitions being added to a table,
/// either at CreateTable or through an UpdateTable `Create` action.
///
/// `existing` are the table's current vector indexes (empty at CreateTable)
/// and `other_index_names` the names already taken by secondary indexes.
pub(crate) fn validate_vector_index_definitions(
    new: &[Value],
    billing_mode: &str,
    attribute_definitions: &[AttributeDefinition],
    existing: &[VectorIndex],
    other_index_names: &std::collections::HashSet<&str>,
) -> Result<(), AwsServiceError> {
    if new.is_empty() {
        return Ok(());
    }
    if billing_mode != "PAY_PER_REQUEST" {
        return Err(invalid(
            "Vector indexes are only supported for PAY_PER_REQUEST tables",
        ));
    }
    if existing.len() + new.len() > MAX_VECTOR_INDEXES_PER_TABLE {
        return Err(invalid(format!(
            "VectorIndex count exceeds the per-table limit of {MAX_VECTOR_INDEXES_PER_TABLE}"
        )));
    }
    let mut names: std::collections::HashSet<&str> = existing
        .iter()
        .map(|v| v.index_name.as_str())
        .chain(other_index_names.iter().copied())
        .collect();
    // Dimensions already fixed per vector attribute.
    let mut dims: HashMap<&str, i64> = existing
        .iter()
        .map(|v| (v.vector_attribute.as_str(), v.dimensions))
        .collect();
    for v in new {
        let name = v["IndexName"].as_str().unwrap_or_default();
        if !names.insert(name) {
            return Err(invalid(format!("Duplicate index name: {name}")));
        }
        let d = v["Dimensions"].as_i64().unwrap_or_default();
        if !(1..=MAX_VECTOR_DIMENSIONS).contains(&d) {
            return Err(invalid(format!(
                "Number of dimensions must be between 1 and {MAX_VECTOR_DIMENSIONS} inclusive."
            )));
        }
        for element in v["SearchSchema"].as_array().into_iter().flatten() {
            let attr = element["AttributeName"].as_str().unwrap_or_default();
            if !attribute_definitions
                .iter()
                .any(|ad| ad.attribute_name == attr)
            {
                return Err(invalid(
                    "One element in SearchSchema is not defined in attribute definitions",
                ));
            }
        }
        let attr = v["VectorAttribute"]["AttributeName"]
            .as_str()
            .unwrap_or_default();
        if let Some(previous) = dims.insert(attr, d) {
            if previous != d {
                return Err(invalid(format!(
                    "Conflicting attribute definition for '{attr}'. All VectorIndexes on the \
                     same vector attribute must use the same dimensions."
                )));
            }
        }
        validate_projection(&v["Projection"])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(err: AwsServiceError) -> String {
        match err {
            AwsServiceError::AwsError { message, .. } => message,
            other => panic!("unexpected error {other:?}"),
        }
    }

    fn base() -> Value {
        json!({
            "TableName": "tbl",
            "AttributeDefinitions": [{"AttributeName": "pk", "AttributeType": "S"}],
            "KeySchema": [{"AttributeName": "pk", "KeyType": "HASH"}],
            "BillingMode": "PAY_PER_REQUEST",
        })
    }

    #[test]
    fn missing_table_name_is_a_required_parameter_error() {
        let mut body = base();
        body.as_object_mut().unwrap().remove("TableName");
        assert_eq!(
            message(validate_create_table_model(&body).unwrap_err()),
            "The parameter 'TableName' is required but was not present in the request"
        );
    }

    #[test]
    fn table_name_errors_are_reported_alone() {
        let mut body = base();
        body["TableName"] = json!("x!");
        body["KeySchema"] = json!([]);
        let msg = message(validate_create_table_model(&body).unwrap_err());
        assert_eq!(
            msg,
            "2 validation errors detected: Value 'x!' at 'tableName' failed to satisfy \
             constraint: Member must have length greater than or equal to 3; Value 'x!' at \
             'tableName' failed to satisfy constraint: Member must satisfy regular expression \
             pattern: [a-zA-Z0-9_.-]+"
        );
    }

    #[test]
    fn member_violations_are_collected() {
        let mut body = base();
        body["KeySchema"] = json!([{"AttributeName": "pk", "KeyType": "INVALID"}]);
        body["BillingMode"] = json!("NOPE");
        let msg = message(validate_create_table_model(&body).unwrap_err());
        assert!(msg.starts_with(
            "2 validation errors detected: Value 'INVALID' at 'keySchema.1.member.keyType'"
        ));
        assert!(msg.contains("Value 'NOPE' at 'billingMode'"));
    }

    #[test]
    fn oversized_key_schema_echoes_the_elements() {
        let mut body = base();
        body["KeySchema"] = json!([
            {"AttributeName": "a", "KeyType": "HASH"},
            {"AttributeName": "b", "KeyType": "RANGE"},
            {"AttributeName": "c", "KeyType": "RANGE"},
        ]);
        assert_eq!(
            message(validate_create_table_model(&body).unwrap_err()),
            "1 validation error detected: Value '[KeySchemaElement(attributeName=a, keyType=HASH), \
             KeySchemaElement(attributeName=b, keyType=RANGE), KeySchemaElement(attributeName=c, \
             keyType=RANGE)]' at 'keySchema' failed to satisfy constraint: Member must have length \
             less than or equal to 2"
        );
    }

    #[test]
    fn unused_attribute_definition_is_rejected() {
        let body = base();
        let ks = parse_key_schema(&body["KeySchema"]).unwrap();
        let defs = vec![
            AttributeDefinition {
                attribute_name: "pk".into(),
                attribute_type: "S".into(),
            },
            AttributeDefinition {
                attribute_name: "extra".into(),
                attribute_type: "N".into(),
            },
        ];
        let msg = message(validate_attribute_definitions_used(&body, &ks, &defs).unwrap_err());
        assert!(
            msg.contains("Some AttributeDefinitions are not used"),
            "{msg}"
        );
    }

    #[test]
    fn vector_index_rules() {
        let defs = vec![AttributeDefinition {
            attribute_name: "pk".into(),
            attribute_type: "S".into(),
        }];
        let vix = |name: &str, dims: i64| {
            json!({
                "IndexName": name,
                "VectorAttribute": {"AttributeName": "embedding"},
                "Dimensions": dims,
                "DistanceFunction": "COSINE",
                "Projection": {"ProjectionType": "ALL"},
            })
        };
        let none = std::collections::HashSet::new();
        let check = |new: Vec<Value>, mode: &str| {
            validate_vector_index_definitions(&new, mode, &defs, &[], &none)
                .err()
                .map(message)
        };
        assert!(check(vec![vix("vix", 3)], "PROVISIONED")
            .unwrap()
            .contains("only supported for PAY_PER_REQUEST"));
        assert!(check(vec![vix("vix", 4097)], "PAY_PER_REQUEST")
            .unwrap()
            .contains("between 1 and 4096"));
        assert!(check(vec![vix("a1", 3), vix("a2", 4)], "PAY_PER_REQUEST")
            .unwrap()
            .contains("Conflicting attribute definition for 'embedding'"));
        let six: Vec<Value> = (0..6).map(|i| vix(&format!("v-{i}"), 3)).collect();
        assert!(check(six, "PAY_PER_REQUEST")
            .unwrap()
            .contains("per-table limit of 5"));
        assert!(check(vec![vix("vix", 3)], "PAY_PER_REQUEST").is_none());
    }
}
