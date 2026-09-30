//! Partition-aware AWS endpoint hostnames.
//!
//! Endpoint hostnames share one shape across partitions; only the DNS suffix
//! differs (`amazonaws.com` for commercial and GovCloud, `amazonaws.com.cn`
//! for China, and each isolated partition its own). Everything that renders
//! or recognizes an AWS hostname derives the suffix from here, so a name
//! minted by one service (a CloudFormation `GetAtt`, an S3 `Location`) is the
//! one another (the CloudFront data plane, the S3 front door) accepts.

use crate::arn::partition_for;

/// The DNS suffix of every AWS partition, without a leading dot. Commercial
/// and GovCloud share `amazonaws.com`, so it appears once.
pub const DNS_SUFFIXES: &[&str] = &[
    "amazonaws.com",
    "amazonaws.com.cn",
    "c2s.ic.gov",
    "sc2s.sgov.gov",
    "cloud.adc-e.uk",
    "csp.hci.ic.gov",
];

/// The DNS suffix of a partition (the `dnsSuffix` of the AWS SDK's partition
/// metadata). `amazonaws.com` for an unknown partition.
pub fn dns_suffix(partition: &str) -> &'static str {
    match partition {
        "aws-cn" => "amazonaws.com.cn",
        "aws-iso" => "c2s.ic.gov",
        "aws-iso-b" => "sc2s.sgov.gov",
        "aws-iso-e" => "cloud.adc-e.uk",
        "aws-iso-f" => "csp.hci.ic.gov",
        _ => "amazonaws.com",
    }
}

/// The DNS suffix of the partition `region` belongs to.
pub fn dns_suffix_for_region(region: &str) -> &'static str {
    dns_suffix(partition_for(region))
}

/// Regions whose S3 static-website endpoint is dash-separated
/// (`s3-website-<region>`). These are the regions that predate the
/// dot-separated form; every region launched since uses `s3-website.<region>`.
const S3_WEBSITE_DASH_REGIONS: &[&str] = &[
    "us-east-1",
    "us-west-1",
    "us-west-2",
    "ap-southeast-1",
    "ap-southeast-2",
    "ap-northeast-1",
    "eu-west-1",
    "sa-east-1",
    "us-gov-west-1",
];

/// The S3 static-website endpoint host of `region`, in the form AWS publishes
/// for it: `s3-website-us-east-1.amazonaws.com` for the legacy regions,
/// `s3-website.eu-central-1.amazonaws.com` for the rest, under the region's
/// partition suffix.
pub fn s3_website_endpoint(region: &str) -> String {
    let sep = if S3_WEBSITE_DASH_REGIONS.contains(&region) {
        '-'
    } else {
        '.'
    };
    format!("s3-website{sep}{region}.{}", dns_suffix_for_region(region))
}

/// A bucket's legacy global endpoint host, `<bucket>.s3.<suffix>` (the
/// `AWS::S3::Bucket` `DomainName` attribute).
pub fn s3_bucket_domain_name(bucket: &str, region: &str) -> String {
    format!("{bucket}.s3.{}", dns_suffix_for_region(region))
}

/// A bucket's regional endpoint host, `<bucket>.s3.<region>.<suffix>` (the
/// `RegionalDomainName` attribute).
pub fn s3_regional_domain_name(bucket: &str, region: &str) -> String {
    format!("{bucket}.s3.{region}.{}", dns_suffix_for_region(region))
}

/// A bucket's dual-stack endpoint host,
/// `<bucket>.s3.dualstack.<region>.<suffix>` (the `DualStackDomainName`
/// attribute).
pub fn s3_dualstack_domain_name(bucket: &str, region: &str) -> String {
    format!(
        "{bucket}.s3.dualstack.{region}.{}",
        dns_suffix_for_region(region)
    )
}

/// A bucket's static-website URL, `http://<bucket>.<website endpoint>` (the
/// `WebsiteURL` attribute). Website endpoints serve HTTP only.
pub fn s3_website_url(bucket: &str, region: &str) -> String {
    format!("http://{bucket}.{}", s3_website_endpoint(region))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arn::PARTITIONS;

    #[test]
    fn dns_suffix_per_partition() {
        for (region, suffix) in [
            ("us-east-1", "amazonaws.com"),
            ("eu-central-1", "amazonaws.com"),
            ("us-gov-west-1", "amazonaws.com"),
            ("cn-north-1", "amazonaws.com.cn"),
            ("cn-northwest-1", "amazonaws.com.cn"),
            ("us-iso-east-1", "c2s.ic.gov"),
            ("us-isob-east-1", "sc2s.sgov.gov"),
            ("eu-isoe-west-1", "cloud.adc-e.uk"),
            ("us-isof-south-1", "csp.hci.ic.gov"),
        ] {
            assert_eq!(dns_suffix_for_region(region), suffix, "{region}");
        }
        assert_eq!(dns_suffix("unknown"), "amazonaws.com");
    }

    #[test]
    fn every_partition_suffix_is_listed() {
        for partition in PARTITIONS {
            assert!(DNS_SUFFIXES.contains(&dns_suffix(partition)), "{partition}");
        }
    }

    #[test]
    fn website_endpoint_dash_form_in_legacy_regions() {
        for region in S3_WEBSITE_DASH_REGIONS {
            assert_eq!(
                s3_website_endpoint(region),
                format!("s3-website-{region}.amazonaws.com")
            );
        }
    }

    #[test]
    fn website_endpoint_dot_form_elsewhere() {
        assert_eq!(
            s3_website_endpoint("eu-central-1"),
            "s3-website.eu-central-1.amazonaws.com"
        );
        assert_eq!(
            s3_website_endpoint("ap-south-1"),
            "s3-website.ap-south-1.amazonaws.com"
        );
        assert_eq!(
            s3_website_endpoint("us-east-2"),
            "s3-website.us-east-2.amazonaws.com"
        );
        assert_eq!(
            s3_website_endpoint("us-gov-east-1"),
            "s3-website.us-gov-east-1.amazonaws.com"
        );
        assert_eq!(
            s3_website_endpoint("cn-north-1"),
            "s3-website.cn-north-1.amazonaws.com.cn"
        );
        assert_eq!(
            s3_website_endpoint("us-iso-east-1"),
            "s3-website.us-iso-east-1.c2s.ic.gov"
        );
    }

    #[test]
    fn bucket_hosts() {
        assert_eq!(
            s3_bucket_domain_name("b", "us-west-2"),
            "b.s3.amazonaws.com"
        );
        assert_eq!(
            s3_bucket_domain_name("b", "cn-north-1"),
            "b.s3.amazonaws.com.cn"
        );
        assert_eq!(
            s3_regional_domain_name("b", "cn-northwest-1"),
            "b.s3.cn-northwest-1.amazonaws.com.cn"
        );
        assert_eq!(
            s3_dualstack_domain_name("b", "us-isob-east-1"),
            "b.s3.dualstack.us-isob-east-1.sc2s.sgov.gov"
        );
        assert_eq!(
            s3_website_url("b", "us-east-1"),
            "http://b.s3-website-us-east-1.amazonaws.com"
        );
        assert_eq!(
            s3_website_url("b", "eu-central-1"),
            "http://b.s3-website.eu-central-1.amazonaws.com"
        );
    }
}
