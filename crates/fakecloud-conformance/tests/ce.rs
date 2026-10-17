//! Cost Explorer (`ce`) awsJson1.1 conformance probe assertions.
//!
//! One `#[test_action]` per operation in the Cost Explorer Smithy model.
//! Checksums are harvested from `aws-models/ce.json`; the probe generates
//! request variants per operation and asserts fakecloud responds AWS-faithfully.

mod helpers;

use fakecloud_conformance_macros::test_action;
use helpers::TestServer;

#[test_action("ce", "CreateAnomalyMonitor", checksum = "0aa3ad69")]
#[test_action("ce", "CreateAnomalySubscription", checksum = "ae097409")]
#[test_action("ce", "CreateCostCategoryDefinition", checksum = "81709300")]
#[test_action("ce", "DeleteAnomalyMonitor", checksum = "a3fa53d7")]
#[test_action("ce", "DeleteAnomalySubscription", checksum = "c5032099")]
#[test_action("ce", "DeleteCostCategoryDefinition", checksum = "29e4da68")]
#[test_action("ce", "DescribeCostCategoryDefinition", checksum = "6e99b32a")]
#[test_action("ce", "GetAnomalies", checksum = "ed1489c4")]
#[test_action("ce", "GetAnomalyMonitors", checksum = "3314b9d7")]
#[test_action("ce", "GetAnomalySubscriptions", checksum = "a7aacd10")]
#[test_action("ce", "GetApproximateUsageRecords", checksum = "1403f771")]
#[test_action("ce", "GetCommitmentPurchaseAnalysis", checksum = "1de674c1")]
#[test_action("ce", "GetCostAndUsage", checksum = "73ad534b")]
#[test_action("ce", "GetCostAndUsageComparisons", checksum = "bdf405d7")]
#[test_action("ce", "GetCostAndUsageWithResources", checksum = "a9dbc789")]
#[test_action("ce", "GetCostCategories", checksum = "5d8dacf0")]
#[test_action("ce", "GetCostComparisonDrivers", checksum = "91ecee5f")]
#[test_action("ce", "GetCostForecast", checksum = "5c69b5df")]
#[test_action("ce", "GetDimensionValues", checksum = "63a37638")]
#[test_action("ce", "GetReservationCoverage", checksum = "aa889b82")]
#[test_action("ce", "GetReservationPurchaseRecommendation", checksum = "0972457e")]
#[test_action("ce", "GetReservationUtilization", checksum = "1342f3a4")]
#[test_action("ce", "GetRightsizingRecommendation", checksum = "afea50de")]
#[test_action(
    "ce",
    "GetSavingsPlanPurchaseRecommendationDetails",
    checksum = "6a75ee52"
)]
#[test_action("ce", "GetSavingsPlansCoverage", checksum = "ad5d934d")]
#[test_action("ce", "GetSavingsPlansPurchaseRecommendation", checksum = "530f3c89")]
#[test_action("ce", "GetSavingsPlansUtilization", checksum = "4519212a")]
#[test_action("ce", "GetSavingsPlansUtilizationDetails", checksum = "beb27db7")]
#[test_action("ce", "GetTags", checksum = "07aaaee7")]
#[test_action("ce", "GetUsageForecast", checksum = "9e10caa4")]
#[test_action("ce", "ListCommitmentPurchaseAnalyses", checksum = "157e17d4")]
#[test_action("ce", "ListCostAllocationTagBackfillHistory", checksum = "ad18fc5d")]
#[test_action("ce", "ListCostAllocationTags", checksum = "e8d57b62")]
#[test_action("ce", "ListCostCategoryDefinitions", checksum = "37c0a9bd")]
#[test_action("ce", "ListCostCategoryResourceAssociations", checksum = "3aa65c91")]
#[test_action(
    "ce",
    "ListSavingsPlansPurchaseRecommendationGeneration",
    checksum = "1a282f7b"
)]
#[test_action("ce", "ListTagsForResource", checksum = "063af854")]
#[test_action("ce", "ProvideAnomalyFeedback", checksum = "c88c4556")]
#[test_action("ce", "StartCommitmentPurchaseAnalysis", checksum = "87ff29d7")]
#[test_action("ce", "StartCostAllocationTagBackfill", checksum = "f45259ee")]
#[test_action(
    "ce",
    "StartSavingsPlansPurchaseRecommendationGeneration",
    checksum = "6b6a60bf"
)]
#[test_action("ce", "TagResource", checksum = "75b89fbc")]
#[test_action("ce", "UntagResource", checksum = "ffe1ddfc")]
#[test_action("ce", "UpdateAnomalyMonitor", checksum = "7bbf5ff9")]
#[test_action("ce", "UpdateAnomalySubscription", checksum = "2edfb5d8")]
#[test_action("ce", "UpdateCostAllocationTagsStatus", checksum = "ecb6b357")]
#[test_action("ce", "UpdateCostCategoryDefinition", checksum = "71f7c5ca")]
#[tokio::test]
async fn ce_probe() {
    let _server = TestServer::start().await;
}
