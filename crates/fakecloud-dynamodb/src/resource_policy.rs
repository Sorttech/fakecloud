//! DynamoDB implementation of [`ResourcePolicyProvider`]: the policy IAM
//! enforcement evaluates alongside the caller's identity policies.
//!
//! A table's policy also governs its indexes (AWS has no separate index
//! policy); a stream has a policy of its own. Backups, exports and imports
//! cannot carry resource-based policies.

use std::sync::Arc;

use fakecloud_core::auth::ResourcePolicyProvider;

use crate::state::SharedDynamoDbState;

pub struct DynamoDbResourcePolicyProvider {
    state: SharedDynamoDbState,
}

impl DynamoDbResourcePolicyProvider {
    pub fn new(state: SharedDynamoDbState) -> Self {
        Self { state }
    }

    /// Convenience constructor for server bootstrap's
    /// `MultiResourcePolicyProvider`.
    pub fn shared(state: SharedDynamoDbState) -> Arc<dyn ResourcePolicyProvider> {
        Arc::new(Self::new(state))
    }
}

/// The account, table name and sub-resource path of a DynamoDB table-scoped
/// ARN: `arn:aws:dynamodb:REGION:ACCOUNT:table/NAME[/KIND/ID]`.
fn parse(arn: &str) -> Option<(&str, &str, Option<&str>)> {
    let rest = arn.strip_prefix("arn:aws:dynamodb:")?;
    let (scope, path) = rest.split_once(":table/")?;
    let account = scope.split(':').nth(1).filter(|a| !a.is_empty())?;
    let (name, sub) = match path.split_once('/') {
        Some((name, sub)) => (name, Some(sub)),
        None => (path, None),
    };
    (!name.is_empty()).then_some((account, name, sub))
}

fn is_dynamodb(service: &str) -> bool {
    service.eq_ignore_ascii_case("dynamodb") || service.eq_ignore_ascii_case("dynamodbstreams")
}

impl ResourcePolicyProvider for DynamoDbResourcePolicyProvider {
    fn resource_policy(&self, service: &str, resource_arn: &str) -> Option<String> {
        if !is_dynamodb(service) {
            return None;
        }
        let (account, name, sub) = parse(resource_arn)?;
        let accounts = self.state.read();
        let state = accounts.get(account)?;
        let table = state.tables.get(name)?;
        match sub {
            None => table.resource_policy.clone(),
            Some(sub) if sub.starts_with("index/") => table.resource_policy.clone(),
            Some(sub) if sub.starts_with("stream/") => {
                state.stream_policies.get(resource_arn).cloned()
            }
            Some(_) => None,
        }
    }

    fn resource_owner_account(&self, service: &str, resource_arn: &str) -> Option<String> {
        if !is_dynamodb(service) {
            return None;
        }
        parse(resource_arn).map(|(account, _, _)| account.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DynamoDbState, DynamoTable, KeySchemaElement, ProvisionedThroughput};
    use fakecloud_core::multi_account::MultiAccountState;

    const ARN: &str = "arn:aws:dynamodb:us-east-1:111122223333:table/Orders";

    fn provider() -> DynamoDbResourcePolicyProvider {
        let mut accounts = MultiAccountState::<DynamoDbState>::new("111122223333", "us-east-1", "");
        let state = accounts.get_or_create("111122223333");
        let mut table = DynamoTable::new(
            "Orders".to_string(),
            ARN.to_string(),
            "id".to_string(),
            vec![KeySchemaElement {
                attribute_name: "pk".to_string(),
                key_type: "HASH".to_string(),
            }],
            vec![],
            ProvisionedThroughput {
                read_capacity_units: 1,
                write_capacity_units: 1,
            },
            "PAY_PER_REQUEST".to_string(),
            chrono::Utc::now(),
        );
        table.resource_policy = Some("table-policy".to_string());
        state.tables.insert("Orders".to_string(), table);
        state
            .stream_policies
            .insert(format!("{ARN}/stream/label"), "stream-policy".to_string());
        DynamoDbResourcePolicyProvider::new(Arc::new(parking_lot::RwLock::new(accounts)))
    }

    #[test]
    fn tables_and_indexes_share_the_table_policy_and_streams_have_their_own() {
        let p = provider();
        assert_eq!(
            p.resource_policy("dynamodb", ARN).as_deref(),
            Some("table-policy")
        );
        assert_eq!(
            p.resource_policy("dynamodb", &format!("{ARN}/index/by-g"))
                .as_deref(),
            Some("table-policy")
        );
        assert_eq!(
            p.resource_policy("dynamodbstreams", &format!("{ARN}/stream/label"))
                .as_deref(),
            Some("stream-policy")
        );
        assert_eq!(
            p.resource_policy("dynamodb", &format!("{ARN}/stream/other")),
            None
        );
        assert_eq!(
            p.resource_policy("dynamodb", &format!("{ARN}/backup/b")),
            None
        );
        assert_eq!(p.resource_policy("sqs", ARN), None);
        assert_eq!(
            p.resource_owner_account(
                "dynamodb",
                "arn:aws:dynamodb:us-east-1:444455556666:table/X"
            )
            .as_deref(),
            Some("444455556666")
        );
    }
}
