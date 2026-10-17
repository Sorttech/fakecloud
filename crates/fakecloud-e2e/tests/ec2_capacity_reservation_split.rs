//! `CreateCapacityReservationBySplitting` / `MoveCapacityReservationInstances`
//! through the real AWS SDK: capacity really leaves the source and lands in
//! the destination, both sides are reported in the response, the new
//! reservation carries only the requested tags, invalid requests get EC2's
//! errors, and the result survives a restart in persistent mode.

mod helpers;

use aws_sdk_ec2::types::{
    CapacityReservationInstancePlatform, ResourceType, Tag, TagSpecification,
};
use helpers::TestServer;

async fn make_cr(ec2: &aws_sdk_ec2::Client, count: i32) -> String {
    ec2.create_capacity_reservation()
        .instance_type("m5.large")
        .instance_platform(CapacityReservationInstancePlatform::LinuxUnix)
        .availability_zone("us-east-1a")
        .instance_count(count)
        .send()
        .await
        .unwrap()
        .capacity_reservation()
        .unwrap()
        .capacity_reservation_id()
        .unwrap()
        .to_string()
}

async fn counts(ec2: &aws_sdk_ec2::Client, id: &str) -> (i32, i32, String) {
    let out = ec2
        .describe_capacity_reservations()
        .capacity_reservation_ids(id)
        .send()
        .await
        .unwrap();
    let r = &out.capacity_reservations()[0];
    (
        r.total_instance_count().unwrap(),
        r.available_instance_count().unwrap(),
        r.state().unwrap().as_str().to_string(),
    )
}

fn code<E: aws_sdk_ec2::error::ProvideErrorMetadata>(e: E) -> String {
    e.code().unwrap_or_default().to_string()
}

#[tokio::test]
async fn split_moves_capacity_and_persists() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let ec2 = server.ec2_client().await;
    let src = make_cr(&ec2, 5).await;

    let out = ec2
        .create_capacity_reservation_by_splitting()
        .source_capacity_reservation_id(&src)
        .instance_count(2)
        .tag_specifications(
            TagSpecification::builder()
                .resource_type(ResourceType::CapacityReservation)
                .tags(Tag::builder().key("stack").value("prod").build())
                .build(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(out.instance_count(), Some(2));
    let source = out.source_capacity_reservation().unwrap();
    assert_eq!(source.capacity_reservation_id(), Some(src.as_str()));
    assert_eq!(source.total_instance_count(), Some(3));
    let dest = out.destination_capacity_reservation().unwrap();
    let dest_id = dest.capacity_reservation_id().unwrap().to_string();
    assert_ne!(dest_id, src);
    assert_eq!(dest.total_instance_count(), Some(2));
    assert_eq!(dest.available_instance_count(), Some(2));
    assert_eq!(dest.instance_type(), Some("m5.large"));
    assert_eq!(dest.availability_zone(), Some("us-east-1a"));
    assert_eq!(dest.tags().len(), 1);
    assert_eq!(dest.tags()[0].key(), Some("stack"));

    // Move one instance back into the source.
    let moved = ec2
        .move_capacity_reservation_instances()
        .source_capacity_reservation_id(&dest_id)
        .destination_capacity_reservation_id(&src)
        .instance_count(1)
        .send()
        .await
        .unwrap();
    assert_eq!(moved.instance_count(), Some(1));
    assert_eq!(
        moved
            .destination_capacity_reservation()
            .unwrap()
            .total_instance_count(),
        Some(4)
    );

    server.restart().await;
    let ec2 = server.ec2_client().await;
    assert_eq!(counts(&ec2, &src).await, (4, 4, "active".to_string()));
    assert_eq!(counts(&ec2, &dest_id).await, (1, 1, "active".to_string()));

    // Moving the rest cancels the now-empty reservation.
    ec2.move_capacity_reservation_instances()
        .source_capacity_reservation_id(&dest_id)
        .destination_capacity_reservation_id(&src)
        .instance_count(1)
        .send()
        .await
        .unwrap();
    assert_eq!(
        counts(&ec2, &dest_id).await,
        (0, 0, "cancelled".to_string())
    );
    assert_eq!(counts(&ec2, &src).await, (5, 5, "active".to_string()));
}

#[tokio::test]
async fn split_rejects_invalid_requests() {
    let server = TestServer::start().await;
    let ec2 = server.ec2_client().await;

    let err = ec2
        .create_capacity_reservation_by_splitting()
        .source_capacity_reservation_id("cr-0123456789abcdef0")
        .instance_count(1)
        .send()
        .await
        .unwrap_err();
    assert_eq!(code(err), "InvalidCapacityReservationId.NotFound");

    let src = make_cr(&ec2, 3).await;
    for n in [0, 3] {
        let err = ec2
            .create_capacity_reservation_by_splitting()
            .source_capacity_reservation_id(&src)
            .instance_count(n)
            .send()
            .await
            .unwrap_err();
        assert_eq!(code(err), "InvalidParameterValue", "instance count {n}");
    }

    let err = ec2
        .create_capacity_reservation_by_splitting()
        .source_capacity_reservation_id(&src)
        .instance_count(1)
        .dry_run(true)
        .send()
        .await
        .unwrap_err();
    assert_eq!(code(err), "DryRunOperation");

    ec2.cancel_capacity_reservation()
        .capacity_reservation_id(&src)
        .send()
        .await
        .unwrap();
    let err = ec2
        .create_capacity_reservation_by_splitting()
        .source_capacity_reservation_id(&src)
        .instance_count(1)
        .send()
        .await
        .unwrap_err();
    assert_eq!(code(err), "IncorrectState");
    // Nothing was taken by any rejected request.
    assert_eq!(counts(&ec2, &src).await, (3, 3, "cancelled".to_string()));
}
