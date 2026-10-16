mod helpers;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::ChecksumAlgorithm;
use helpers::TestServer;

/// Bug-hunt 1.13: PutObject computed the modern `x-amz-checksum-*` value over
/// the received body but never compared it against a client-supplied value, so
/// a corrupt upload with a wrong precomputed checksum stored silently. AWS
/// rejects the mismatch with `BadDigest`. Supplying an explicit (wrong)
/// SHA256 must fail; a correct request still succeeds.
#[tokio::test]
async fn s3_put_object_rejects_checksum_mismatch() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;

    s3.create_bucket()
        .bucket("checksum-bucket")
        .send()
        .await
        .unwrap();

    // A syntactically-valid base64 SHA256 that does NOT match the body.
    let wrong_sha256 = "3q2+7wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    let result = s3
        .put_object()
        .bucket("checksum-bucket")
        .key("obj")
        .body(ByteStream::from_static(b"hello world"))
        .checksum_sha256(wrong_sha256)
        .send()
        .await;
    assert!(
        result.is_err(),
        "a mismatched x-amz-checksum-sha256 must be rejected"
    );

    // Letting the SDK compute the checksum for us round-trips fine.
    s3.put_object()
        .bucket("checksum-bucket")
        .key("obj-ok")
        .body(ByteStream::from_static(b"hello world"))
        .checksum_algorithm(ChecksumAlgorithm::Sha256)
        .send()
        .await
        .expect("a correct checksum must be accepted");
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Bug-hunt 5.4: a plain (non-chunked) SigV4 PutObject carries the payload's
/// real SHA-256 in `x-amz-content-sha256`. fakecloud spooled the body but never
/// compared this header against the bytes received, so a corrupt/tampered
/// upload whose header disagreed with its body stored silently. AWS rejects the
/// divergence with `XAmzContentSHA256Mismatch` (400). This is enforced at the
/// S3 layer (the buffered payload is authoritative) rather than in sigv4
/// verification, where aws-chunked/streaming empties the body buffer.
#[tokio::test]
async fn s3_put_object_rejects_content_sha256_mismatch() {
    let s = TestServer::start().await;
    let s3 = s.s3_client().await;
    s3.create_bucket()
        .bucket("sha-bucket")
        .send()
        .await
        .unwrap();

    let body = b"hello world".to_vec();
    let bogus = "0".repeat(64); // valid hex, wrong for the body

    // Raw PUT with a SigV4 Authorization header (routes through the streaming
    // spool path; signature is not verified — auth is off by default) and a
    // mismatched x-amz-content-sha256 hex digest.
    let url = format!("{}/sha-bucket/obj", s.endpoint());
    let resp = reqwest::Client::new()
        .put(&url)
        .header(
            "authorization",
            "AWS4-HMAC-SHA256 Credential=test/20240101/us-east-1/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256, Signature=0000000000000000000000000000000000000000000000000000000000000000",
        )
        .header("x-amz-content-sha256", &bogus)
        .header("content-length", body.len().to_string())
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "a mismatched x-amz-content-sha256 must be rejected with 400"
    );
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("XAmzContentSHA256Mismatch"),
        "expected XAmzContentSHA256Mismatch, got: {text}"
    );

    // The correct digest for the same body round-trips fine.
    let good = sha256_hex(&body);
    let ok = reqwest::Client::new()
        .put(&url)
        .header(
            "authorization",
            "AWS4-HMAC-SHA256 Credential=test/20240101/us-east-1/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256, Signature=0000000000000000000000000000000000000000000000000000000000000000",
        )
        .header("x-amz-content-sha256", &good)
        .header("content-length", body.len().to_string())
        .body(body)
        .send()
        .await
        .unwrap();
    assert!(
        ok.status().is_success(),
        "a correct x-amz-content-sha256 must be accepted, got {}",
        ok.status()
    );

    // UNSIGNED-PAYLOAD must skip the check entirely (marker, not a body hash).
    let unsigned = reqwest::Client::new()
        .put(format!("{}/sha-bucket/obj2", s.endpoint()))
        .header(
            "authorization",
            "AWS4-HMAC-SHA256 Credential=test/20240101/us-east-1/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256, Signature=0000000000000000000000000000000000000000000000000000000000000000",
        )
        .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
        .body(b"anything".to_vec())
        .send()
        .await
        .unwrap();
    assert!(
        unsigned.status().is_success(),
        "UNSIGNED-PAYLOAD must bypass the content-sha256 check, got {}",
        unsigned.status()
    );
}

// ── Flexible checksums: every algorithm, trailers, multipart, annotations ──

use aws_sdk_s3::types::{
    ChecksumMode, ChecksumType, CompletedMultipartUpload, CompletedPart, ObjectAttributes,
};

/// Reference checksums of `b"hello world"`, computed independently of
/// fakecloud (python `zlib`/`hashlib`, the python-xxhash binding of the
/// reference C library with seed 0, and a bitwise CRC-64/NVME whose
/// "123456789" check value is the catalogue's 0xAE8B14860A799888), each as
/// base64 of the big-endian digest.
const HELLO: &[u8] = b"hello world";
const HELLO_CHECKSUMS: [(&str, &str); 10] = [
    ("crc32", "DUoRhQ=="),
    ("crc32c", "yZRlqg=="),
    ("crc64nvme", "jSnVw/bqjr4="),
    ("sha1", "Kq5sNclPz7QV2+lfQIuc6R7oRu0="),
    ("sha256", "uU0nuZNNPgilLlLX2n2r+sSE7+N6U4DukIj3rOLvzek="),
    (
        "sha512",
        "MJ7MSJwS1utMxA9QyQLytNDtd+5RGnx6m808qG1M2G+YndNbxf9JlnDaNCVbRbDP2DDoH2Bdz33FVC6TrpzXbw==",
    ),
    ("md5", "XrY7u+Ae7tCTyyK7j1rNww=="),
    ("xxhash64", "RatnNLIeaWg="),
    ("xxhash3", "1Eex6kDmmIs="),
    ("xxhash128", "340J6T+HSQCpm4d1zBW2xw=="),
];

fn hello_checksum(algo: &str) -> &'static str {
    HELLO_CHECKSUMS
        .iter()
        .find(|(a, _)| *a == algo)
        .map(|(_, v)| *v)
        .unwrap()
}

/// A SigV4-shaped Authorization header: routes a raw request to S3 (and
/// through the streaming spool for uploads). Signatures are not verified.
const FAKE_AUTH: &str = "AWS4-HMAC-SHA256 Credential=test/20240101/us-east-1/s3/aws4_request, \
     SignedHeaders=host;x-amz-content-sha256, Signature=0000000000000000000000000000000000000000000000000000000000000000";

fn raw() -> reqwest::Client {
    reqwest::Client::new()
}

/// An unsigned aws-chunked body carrying `payload` and one trailer line.
fn chunked_with_trailer(payload: &[u8], trailer_name: &str, trailer_value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for c in payload.chunks(8192) {
        out.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
        out.extend_from_slice(c);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("0\r\n{trailer_name}:{trailer_value}\r\n\r\n").as_bytes());
    out
}

#[tokio::test]
async fn s3_sdk_checksum_algorithms_round_trip_with_checksum_mode() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    s3.create_bucket().bucket("ck-sdk").send().await.unwrap();

    let cases = [
        (ChecksumAlgorithm::Crc32, "crc32"),
        (ChecksumAlgorithm::Crc32C, "crc32c"),
        (ChecksumAlgorithm::Crc64Nvme, "crc64nvme"),
        (ChecksumAlgorithm::Sha1, "sha1"),
        (ChecksumAlgorithm::Sha256, "sha256"),
    ];
    for (algo, name) in cases {
        let expected = hello_checksum(name);
        let put = s3
            .put_object()
            .bucket("ck-sdk")
            .key(name)
            .body(ByteStream::from_static(HELLO))
            .checksum_algorithm(algo.clone())
            .send()
            .await
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let put_value = match name {
            "crc32" => put.checksum_crc32(),
            "crc32c" => put.checksum_crc32_c(),
            "crc64nvme" => put.checksum_crc64_nvme(),
            "sha1" => put.checksum_sha1(),
            _ => put.checksum_sha256(),
        };
        assert_eq!(put_value, Some(expected), "{name} PutObject");
        assert_eq!(put.checksum_type(), Some(&ChecksumType::FullObject));

        // With checksum mode enabled the SDK also validates the body against
        // the returned checksum, so a wrong value fails this call.
        let got = s3
            .get_object()
            .bucket("ck-sdk")
            .key(name)
            .checksum_mode(ChecksumMode::Enabled)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let got_value = match name {
            "crc32" => got.checksum_crc32().map(str::to_string),
            "crc32c" => got.checksum_crc32_c().map(str::to_string),
            "crc64nvme" => got.checksum_crc64_nvme().map(str::to_string),
            "sha1" => got.checksum_sha1().map(str::to_string),
            _ => got.checksum_sha256().map(str::to_string),
        };
        assert_eq!(got_value.as_deref(), Some(expected), "{name} GetObject");
        assert_eq!(got.checksum_type(), Some(&ChecksumType::FullObject));
        let body = got.body.collect().await.unwrap().into_bytes();
        assert_eq!(&body[..], HELLO);

        let head = s3
            .head_object()
            .bucket("ck-sdk")
            .key(name)
            .checksum_mode(ChecksumMode::Enabled)
            .send()
            .await
            .unwrap();
        let head_value = match name {
            "crc32" => head.checksum_crc32(),
            "crc32c" => head.checksum_crc32_c(),
            "crc64nvme" => head.checksum_crc64_nvme(),
            "sha1" => head.checksum_sha1(),
            _ => head.checksum_sha256(),
        };
        assert_eq!(head_value, Some(expected), "{name} HeadObject");
        assert_eq!(head.checksum_type(), Some(&ChecksumType::FullObject));
    }

    // Listing reports each object's algorithm and type.
    let list = s3.list_objects_v2().bucket("ck-sdk").send().await.unwrap();
    for obj in list.contents() {
        assert_eq!(obj.checksum_type(), Some(&ChecksumType::FullObject));
        assert_eq!(obj.checksum_algorithm().len(), 1);
    }
}

#[tokio::test]
async fn s3_raw_put_supports_every_checksum_algorithm() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    s3.create_bucket().bucket("ck-raw").send().await.unwrap();
    let base = format!("{}/ck-raw", server.endpoint());

    for (name, value) in HELLO_CHECKSUMS {
        let header = format!("x-amz-checksum-{name}");
        let resp = raw()
            .put(format!("{base}/{name}"))
            .header("authorization", FAKE_AUTH)
            .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
            .header(&header, value)
            .body(HELLO.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{name}");
        assert_eq!(resp.headers()[header.as_str()], value, "{name}");

        let got = raw()
            .get(format!("{base}/{name}"))
            .header("authorization", FAKE_AUTH)
            .header("x-amz-checksum-mode", "ENABLED")
            .send()
            .await
            .unwrap();
        assert_eq!(got.status(), 200);
        assert_eq!(got.headers()[header.as_str()], value, "{name}");
        assert_eq!(got.headers()["x-amz-checksum-type"], "FULL_OBJECT");

        // A digest that does not match the body is BadDigest.
        let wrong = raw()
            .put(format!("{base}/{name}-bad"))
            .header("authorization", FAKE_AUTH)
            .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
            .header(&header, value)
            .body(b"hello world!".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 400, "{name}");
        assert!(wrong.text().await.unwrap().contains("BadDigest"), "{name}");
    }

    // No checksum at all: S3's CRC64NVME default.
    raw()
        .put(format!("{base}/plain"))
        .header("authorization", FAKE_AUTH)
        .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
        .body(HELLO.to_vec())
        .send()
        .await
        .unwrap();
    let head = s3
        .head_object()
        .bucket("ck-raw")
        .key("plain")
        .checksum_mode(ChecksumMode::Enabled)
        .send()
        .await
        .unwrap();
    assert_eq!(
        head.checksum_crc64_nvme(),
        Some(hello_checksum("crc64nvme"))
    );

    // An algorithm outside the enum is rejected.
    let bad = raw()
        .put(format!("{base}/bad-algo"))
        .header("authorization", FAKE_AUTH)
        .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
        .header("x-amz-sdk-checksum-algorithm", "CRC16")
        .body(HELLO.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    assert!(bad.text().await.unwrap().contains("InvalidRequest"));
}

#[tokio::test]
async fn s3_trailing_checksum_upload_is_verified() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    s3.create_bucket()
        .bucket("ck-trailer")
        .send()
        .await
        .unwrap();
    let payload: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
    let sha512 = {
        use base64::Engine;
        use sha2::{Digest, Sha512};
        base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&payload))
    };

    let put = |key: &str, trailer_value: &str| {
        let body = chunked_with_trailer(&payload, "x-amz-checksum-sha512", trailer_value);
        raw()
            .put(format!("{}/ck-trailer/{key}", server.endpoint()))
            .header("authorization", FAKE_AUTH)
            .header("content-encoding", "aws-chunked")
            .header("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER")
            .header("x-amz-decoded-content-length", payload.len().to_string())
            .header("x-amz-trailer", "x-amz-checksum-sha512")
            .header("x-amz-sdk-checksum-algorithm", "SHA512")
            .body(body)
            .send()
    };
    let ok = put("good", &sha512).await.unwrap();
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.headers()["x-amz-checksum-sha512"], sha512.as_str());
    let got = s3
        .get_object()
        .bucket("ck-trailer")
        .key("good")
        .send()
        .await
        .unwrap();
    assert_eq!(
        &got.body.collect().await.unwrap().into_bytes()[..],
        &payload[..]
    );

    let bad = put("bad", hello_checksum("sha512")).await.unwrap();
    assert_eq!(bad.status(), 400);
    assert!(bad.text().await.unwrap().contains("BadDigest"));
    assert!(s3
        .head_object()
        .bucket("ck-trailer")
        .key("bad")
        .send()
        .await
        .is_err());

    // The SDK's own streaming upload (aws-chunked with a trailing CRC32C)
    // round-trips through the same path.
    let put = s3
        .put_object()
        .bucket("ck-trailer")
        .key("sdk")
        .body(ByteStream::from(payload.clone()))
        .checksum_algorithm(ChecksumAlgorithm::Crc32C)
        .send()
        .await
        .unwrap();
    let head = s3
        .head_object()
        .bucket("ck-trailer")
        .key("sdk")
        .checksum_mode(ChecksumMode::Enabled)
        .send()
        .await
        .unwrap();
    assert_eq!(head.checksum_crc32_c(), put.checksum_crc32_c());
    assert!(head.checksum_crc32_c().is_some());
}

#[tokio::test]
async fn s3_multipart_composite_and_full_object_checksums() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    s3.create_bucket().bucket("ck-mpu").send().await.unwrap();
    let part1 = vec![b'a'; 5 * 1024 * 1024];
    let part2 = b"the last part".to_vec();

    // COMPOSITE SHA256.
    let create = s3
        .create_multipart_upload()
        .bucket("ck-mpu")
        .key("composite")
        .checksum_algorithm(ChecksumAlgorithm::Sha256)
        .send()
        .await
        .unwrap();
    assert_eq!(create.checksum_type(), Some(&ChecksumType::Composite));
    let upload_id = create.upload_id().unwrap().to_string();
    let mut parts = Vec::new();
    for (n, data) in [(1, &part1), (2, &part2)] {
        let r = s3
            .upload_part()
            .bucket("ck-mpu")
            .key("composite")
            .upload_id(&upload_id)
            .part_number(n)
            .checksum_algorithm(ChecksumAlgorithm::Sha256)
            .body(ByteStream::from(data.clone()))
            .send()
            .await
            .unwrap();
        parts.push(
            CompletedPart::builder()
                .part_number(n)
                .e_tag(r.e_tag().unwrap())
                .checksum_sha256(r.checksum_sha256().expect("part checksum"))
                .build(),
        );
    }
    let done = s3
        .complete_multipart_upload()
        .bucket("ck-mpu")
        .key("composite")
        .upload_id(&upload_id)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .set_parts(Some(parts.clone()))
                .build(),
        )
        .send()
        .await
        .unwrap();
    let composite = done.checksum_sha256().unwrap().to_string();
    assert!(composite.ends_with("-2"), "{composite}");
    assert_eq!(done.checksum_type(), Some(&ChecksumType::Composite));

    let attrs = s3
        .get_object_attributes()
        .bucket("ck-mpu")
        .key("composite")
        .object_attributes(ObjectAttributes::Checksum)
        .object_attributes(ObjectAttributes::ObjectParts)
        .send()
        .await
        .unwrap();
    let checksum = attrs.checksum().unwrap();
    assert_eq!(checksum.checksum_sha256(), Some(composite.as_str()));
    assert_eq!(checksum.checksum_type(), Some(&ChecksumType::Composite));
    let object_parts = attrs.object_parts().unwrap();
    assert_eq!(object_parts.total_parts_count(), Some(2));
    assert_eq!(
        object_parts.parts()[1].checksum_sha256(),
        parts[1].checksum_sha256()
    );
    // A part-level GET returns (and the SDK validates) that part's checksum.
    let part_get = s3
        .get_object()
        .bucket("ck-mpu")
        .key("composite")
        .part_number(2)
        .checksum_mode(ChecksumMode::Enabled)
        .send()
        .await
        .unwrap();
    assert_eq!(part_get.checksum_sha256(), parts[1].checksum_sha256());
    let bytes = part_get.body.collect().await.unwrap().into_bytes();
    assert_eq!(&bytes[..], &part2[..]);

    // FULL_OBJECT CRC64NVME with the whole-object value and size asserted.
    let create = s3
        .create_multipart_upload()
        .bucket("ck-mpu")
        .key("full")
        .checksum_algorithm(ChecksumAlgorithm::Crc64Nvme)
        .checksum_type(ChecksumType::FullObject)
        .send()
        .await
        .unwrap();
    let upload_id = create.upload_id().unwrap().to_string();
    let mut parts = Vec::new();
    for (n, data) in [(1, &part1), (2, &part2)] {
        let r = s3
            .upload_part()
            .bucket("ck-mpu")
            .key("full")
            .upload_id(&upload_id)
            .part_number(n)
            .checksum_algorithm(ChecksumAlgorithm::Crc64Nvme)
            .body(ByteStream::from(data.clone()))
            .send()
            .await
            .unwrap();
        parts.push(
            CompletedPart::builder()
                .part_number(n)
                .e_tag(r.e_tag().unwrap())
                .checksum_crc64_nvme(r.checksum_crc64_nvme().unwrap())
                .build(),
        );
    }
    // The whole object's CRC64NVME, taken from a single-part PUT of the same
    // bytes.
    let mut whole = part1.clone();
    whole.extend_from_slice(&part2);
    let single = s3
        .put_object()
        .bucket("ck-mpu")
        .key("single")
        .body(ByteStream::from(whole.clone()))
        .checksum_algorithm(ChecksumAlgorithm::Crc64Nvme)
        .send()
        .await
        .unwrap();
    let whole_crc = single.checksum_crc64_nvme().unwrap().to_string();
    let wrong = s3
        .complete_multipart_upload()
        .bucket("ck-mpu")
        .key("full")
        .upload_id(&upload_id)
        .checksum_crc64_nvme(hello_checksum("crc64nvme"))
        .checksum_type(ChecksumType::FullObject)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .set_parts(Some(parts.clone()))
                .build(),
        )
        .send()
        .await
        .unwrap_err();
    assert_eq!(wrong.into_service_error().meta().code(), Some("BadDigest"));
    let done = s3
        .complete_multipart_upload()
        .bucket("ck-mpu")
        .key("full")
        .upload_id(&upload_id)
        .checksum_crc64_nvme(&whole_crc)
        .checksum_type(ChecksumType::FullObject)
        .mpu_object_size(whole.len() as i64)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .set_parts(Some(parts))
                .build(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(done.checksum_crc64_nvme(), Some(whole_crc.as_str()));
    assert_eq!(done.checksum_type(), Some(&ChecksumType::FullObject));
    let head = s3
        .head_object()
        .bucket("ck-mpu")
        .key("full")
        .checksum_mode(ChecksumMode::Enabled)
        .send()
        .await
        .unwrap();
    assert_eq!(head.checksum_crc64_nvme(), Some(whole_crc.as_str()));
    assert_eq!(head.checksum_type(), Some(&ChecksumType::FullObject));
}

#[tokio::test]
async fn s3_object_annotation_checksums_and_trailer() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    s3.create_bucket().bucket("ck-annot").send().await.unwrap();
    s3.put_object()
        .bucket("ck-annot")
        .key("doc")
        .body(ByteStream::from_static(b"object body"))
        .send()
        .await
        .unwrap();
    let url = |name: &str| {
        format!(
            "{}/ck-annot/doc?annotation&annotationName={name}",
            server.endpoint()
        )
    };

    // Streamed the way an SDK sends PutObjectAnnotation: aws-chunked framing
    // with the checksum as a trailer.
    let body = chunked_with_trailer(
        HELLO,
        "x-amz-checksum-xxhash128",
        hello_checksum("xxhash128"),
    );
    let put = raw()
        .put(url("review"))
        .header("authorization", FAKE_AUTH)
        .header("content-encoding", "aws-chunked")
        .header("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER")
        .header("x-amz-decoded-content-length", HELLO.len().to_string())
        .header("x-amz-trailer", "x-amz-checksum-xxhash128")
        .header("x-amz-sdk-checksum-algorithm", "XXHASH128")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);
    assert_eq!(
        put.headers()["x-amz-checksum-xxhash128"],
        hello_checksum("xxhash128")
    );
    assert_eq!(put.headers()["x-amz-checksum-type"], "FULL_OBJECT");

    let got = raw()
        .get(format!("{}&x-id=GetObjectAnnotation", url("review")))
        .header("authorization", FAKE_AUTH)
        .header("x-amz-checksum-mode", "ENABLED")
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    assert_eq!(
        got.headers()["x-amz-checksum-xxhash128"],
        hello_checksum("xxhash128")
    );
    assert_eq!(got.headers()["x-amz-checksum-type"], "FULL_OBJECT");
    // The stored payload is the decoded body, not the framing.
    assert_eq!(&got.bytes().await.unwrap()[..], HELLO);

    // A trailer that does not match the payload is BadDigest.
    let body = chunked_with_trailer(HELLO, "x-amz-checksum-xxhash128", hello_checksum("md5"));
    let bad = raw()
        .put(url("tampered"))
        .header("authorization", FAKE_AUTH)
        .header("content-encoding", "aws-chunked")
        .header("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER")
        .header("x-amz-trailer", "x-amz-checksum-xxhash128")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    assert!(bad.text().await.unwrap().contains("BadDigest"));
}

#[tokio::test]
async fn s3_buffered_request_body_checksums_are_verified() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    s3.create_bucket().bucket("ck-body").send().await.unwrap();
    // The SDK checksums PutBucketTagging's body itself; a correct request
    // passes.
    s3.put_bucket_tagging()
        .bucket("ck-body")
        .tagging(
            aws_sdk_s3::types::Tagging::builder()
                .tag_set(
                    aws_sdk_s3::types::Tag::builder()
                        .key("k")
                        .value("v")
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    // A Content-MD5 that is not the body's is BadDigest.
    let resp = raw()
        .put(format!("{}/ck-body?tagging", server.endpoint()))
        .header("authorization", FAKE_AUTH)
        .header("content-md5", hello_checksum("md5"))
        .body("<Tagging><TagSet></TagSet></Tagging>")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().await.unwrap().contains("BadDigest"));
}
