use std::fmt;

/// An Amazon Resource Name.
///
/// Format: `arn:partition:service:region:account-id:resource`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Arn {
    pub partition: String,
    pub service: String,
    pub region: String,
    pub account_id: String,
    pub resource: String,
}

impl Arn {
    pub fn new(service: &str, region: &str, account_id: &str, resource: &str) -> Self {
        Self {
            partition: "aws".to_string(),
            service: service.to_string(),
            region: region.to_string(),
            account_id: account_id.to_string(),
            resource: resource.to_string(),
        }
    }

    /// Create an ARN with no region (global services like IAM).
    pub fn global(service: &str, account_id: &str, resource: &str) -> Self {
        Self::new(service, "", account_id, resource)
    }

    /// Create an S3 ARN — no region, no account.
    /// Format: `arn:aws:s3:::resource`.
    pub fn s3(resource: &str) -> Self {
        Self {
            partition: "aws".to_string(),
            service: "s3".to_string(),
            region: String::new(),
            account_id: String::new(),
            resource: resource.to_string(),
        }
    }

    /// Create an S3 Access Point ARN.
    /// Format: `arn:aws:s3:<region>:<account-id>:accesspoint/<name>`.
    pub fn s3_access_point(region: &str, account_id: &str, name: &str) -> Self {
        Self {
            partition: partition_for(region).to_string(),
            service: "s3".to_string(),
            region: region.to_string(),
            account_id: account_id.to_string(),
            resource: format!("accesspoint/{name}"),
        }
    }

    /// Override the partition (default `aws`). Use for `aws-cn` / `aws-us-gov`.
    pub fn with_partition(mut self, partition: &str) -> Self {
        self.partition = partition.to_string();
        self
    }
}

/// An AWS unique id derived from a resource ARN: the 4-char prefix AWS uses
/// for that resource family (`AIPA` for an instance profile, `AIDA` for a
/// user, `AROA` for a role) followed by 17 uppercase base32 characters, the
/// 21-character shape AWS returns.
///
/// Deriving it from the ARN rather than minting it randomly lets two services
/// that both report the same resource's id agree on it without sharing state:
/// IAM reports the instance profile's `InstanceProfileId`, and EC2 reports the
/// same value on every instance the profile is attached to.
///
/// The trade-off is that the id is a pure function of the ARN, so deleting a
/// resource and creating it again under the same name returns the same id
/// where AWS would mint a fresh one. Id inequality is therefore not a reliable
/// "different resource" signal here.
pub fn unique_id_for(prefix: &str, arn: &str) -> String {
    // FNV-1a over the ARN, so the id is stable across restarts and processes
    // (a random value, or one from a seeded hasher, is not).
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in arn.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // RFC 4648 base32: the uppercase letters plus 2-7, which is the character
    // set AWS's unique ids use.
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut suffix = String::with_capacity(17);
    for i in 0..17 {
        // Stir between characters so every one varies with the whole hash.
        let shifted = hash.rotate_left(i * 5);
        suffix.push(ALPHABET[(shifted % ALPHABET.len() as u64) as usize] as char);
    }
    format!("{prefix}{suffix}")
}

/// Map an AWS region name to its partition. Mirrors the AWS SDK's
/// region-to-partition lookup so synthesized ARNs in cn/gov-cloud and the
/// isolated regions emit the correct partition prefix.
pub fn partition_for(region: &str) -> &'static str {
    if region.starts_with("cn-") {
        "aws-cn"
    } else if region.starts_with("us-gov-") {
        "aws-us-gov"
    } else if region.starts_with("us-iso-") {
        "aws-iso"
    } else if region.starts_with("us-isob-") {
        "aws-iso-b"
    } else if region.starts_with("us-isof-") {
        "aws-iso-f"
    } else if region.starts_with("eu-isoe-") {
        "aws-iso-e"
    } else {
        "aws"
    }
}

impl fmt::Display for Arn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "arn:{}:{}:{}:{}:{}",
            self.partition, self.service, self.region, self.account_id, self.resource
        )
    }
}

impl std::str::FromStr for Arn {
    type Err = ArnParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.splitn(6, ':').collect();
        if parts.len() != 6 || parts[0] != "arn" {
            return Err(ArnParseError(s.to_string()));
        }
        Ok(Self {
            partition: parts[1].to_string(),
            service: parts[2].to_string(),
            region: parts[3].to_string(),
            account_id: parts[4].to_string(),
            resource: parts[5].to_string(),
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid ARN: {0}")]
pub struct ArnParseError(String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let arn = Arn::new("sqs", "us-east-1", "123456789012", "my-queue");
        let s = arn.to_string();
        assert_eq!(s, "arn:aws:sqs:us-east-1:123456789012:my-queue");
        assert_eq!(s.parse::<Arn>().unwrap(), arn);
    }

    #[test]
    fn global_arn() {
        let arn = Arn::global("iam", "123456789012", "user/admin");
        assert_eq!(arn.to_string(), "arn:aws:iam::123456789012:user/admin");
    }

    #[test]
    fn s3_arn() {
        let arn = Arn::s3("my-bucket");
        assert_eq!(arn.to_string(), "arn:aws:s3:::my-bucket");
        let object = Arn::s3("my-bucket/key.txt");
        assert_eq!(object.to_string(), "arn:aws:s3:::my-bucket/key.txt");
    }

    #[test]
    fn with_partition_overrides() {
        let arn = Arn::new("sqs", "cn-north-1", "123", "q").with_partition("aws-cn");
        assert_eq!(arn.to_string(), "arn:aws-cn:sqs:cn-north-1:123:q");
    }

    #[test]
    fn unique_id_is_stable_and_aws_shaped() {
        let arn = "arn:aws:iam::123456789012:instance-profile/web";
        let id = unique_id_for("AIPA", arn);
        assert_eq!(id.len(), 21, "{id}");
        assert!(id.starts_with("AIPA"), "{id}");
        assert!(
            id[4..]
                .chars()
                .all(|c| c.is_ascii_uppercase() || ('2'..='7').contains(&c)),
            "{id}"
        );
        // Same ARN -> same id; a different ARN -> a different one.
        assert_eq!(id, unique_id_for("AIPA", arn));
        assert_ne!(
            id,
            unique_id_for("AIPA", "arn:aws:iam::123456789012:instance-profile/other")
        );
        // The partition is part of the ARN, so it is part of the id.
        assert_ne!(
            id,
            unique_id_for("AIPA", "arn:aws-cn:iam::123456789012:instance-profile/web")
        );
    }

    #[test]
    fn partition_for_region() {
        assert_eq!(partition_for("us-east-1"), "aws");
        assert_eq!(partition_for("eu-west-1"), "aws");
        assert_eq!(partition_for("cn-north-1"), "aws-cn");
        assert_eq!(partition_for("cn-northwest-1"), "aws-cn");
        assert_eq!(partition_for("us-gov-west-1"), "aws-us-gov");
        assert_eq!(partition_for("us-iso-east-1"), "aws-iso");
        assert_eq!(partition_for("us-isob-east-1"), "aws-iso-b");
        assert_eq!(partition_for("us-isof-south-1"), "aws-iso-f");
        assert_eq!(partition_for("eu-isoe-west-1"), "aws-iso-e");
    }
}
