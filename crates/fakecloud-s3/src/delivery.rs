use std::sync::Arc;

use bytes::Bytes;
use chrono::Utc;
use fakecloud_core::delivery::S3Delivery;
use fakecloud_persistence::S3Store;
use md5::{Digest, Md5};
use parking_lot::RwLock;

use crate::state::{memory_body, AclGrant, S3Object, SharedS3State};

/// Lets non-S3 services write objects to buckets without taking a
/// direct dep on this crate. Used by CloudWatch Logs export tasks,
/// CloudWatch Logs deliveries, Firehose S3 destinations, ELB access
/// logs, and similar producers.
pub struct S3DeliveryImpl {
    state: SharedS3State,
    /// Durable S3 backing store. S3 durability is write-through to this store
    /// (there is no `s3_state` snapshot; on boot S3 is rebuilt from the store),
    /// so an object written only into `state` is lost on restart. Behind a lock
    /// so callers built before the S3 store exists during startup can wire it up
    /// later via [`Self::set_s3_store`]. `None` => memory mode (no store).
    store: RwLock<Option<Arc<dyn S3Store>>>,
}

impl S3DeliveryImpl {
    pub fn new(state: SharedS3State) -> Self {
        Self {
            state,
            store: RwLock::new(None),
        }
    }

    /// Wire the durable S3 store after construction, so delivered objects are
    /// written through to disk and survive a restart. Several producers (the
    /// CloudWatch-Logs delivery bus, ELB access logs) build this impl before the
    /// S3 store exists during server startup, so the store is set later.
    pub fn set_s3_store(&self, store: Arc<dyn S3Store>) {
        *self.store.write() = Some(store);
    }
}

/// Write `body` as `bucket/key` in `account_id`, persisting through `store`
/// when one is configured (disk mode). Shared by [`S3DeliveryImpl`] and
/// services that write result objects (Athena query results). See
/// [`put_delivered_object`] for the write semantics.
pub fn put_object_via_store(
    state: &SharedS3State,
    store: Option<&dyn S3Store>,
    account_id: &str,
    bucket: &str,
    key: &str,
    body: Vec<u8>,
    content_type: Option<&str>,
) -> Result<(), String> {
    let bytes = Bytes::from(body);
    let obj = S3Object {
        key: key.to_string(),
        content_type: content_type
            .unwrap_or("application/octet-stream")
            .to_string(),
        etag: format!("{:x}", Md5::digest(&bytes)),
        size: bytes.len() as u64,
        ..Default::default()
    };
    put_delivered_object(state, store, account_id, bucket, obj, bytes).map(|_| ())
}

/// Write an object another service produced (a log export, a delivery
/// stream batch, an access log, a query result, ...) into `bucket` in
/// `account_id`. `obj` carries the object's metadata (key, content type,
/// ETag, size, encryption); `body` is the stored payload.
///
/// On AWS such an object is an ordinary new object version, so it is written
/// exactly as PutObject writes one: the bucket's versioning decides its
/// version id and history, and on an Object Lock bucket the default retention
/// applies. The body is persisted through `store` when one is configured, so
/// the object (and every version it displaces) survives a restart. A missing
/// bucket or a failed store write is an error, and leaves the bucket's
/// in-memory state unchanged.
///
/// The payload is staged next to the store with no S3 lock held; the global
/// S3 write lock is taken only for the rename-into-place and the in-memory
/// commit. Returns the new version id on a versioning-enabled bucket.
pub fn put_delivered_object(
    state: &SharedS3State,
    store: Option<&dyn S3Store>,
    account_id: &str,
    bucket: &str,
    mut obj: S3Object,
    body: Bytes,
) -> Result<Option<String>, String> {
    let key = obj.key.clone();
    let missing = || format!("bucket {bucket} not found in account {account_id}");

    // Cheap existence check under a read guard, so a delivery to a missing
    // bucket never touches the disk.
    {
        let accounts = state.read();
        let exists = accounts
            .get(account_id)
            .is_some_and(|s| s.buckets.contains_key(bucket));
        if !exists {
            return Err(missing());
        }
    }

    // Every new object carries an additional checksum, as on S3: the one the
    // producer set (it must, for an encrypted payload, since the checksum
    // covers the plaintext), else S3's default CRC64NVME full-object checksum
    // of the body, the one PutObject stores for an upload that names none.
    if obj.checksum_value.is_none() && obj.sse_algorithm.as_deref() != Some("aws:kms") {
        let algorithm = obj
            .checksum_algorithm
            .unwrap_or(crate::checksum::DEFAULT_ALGORITHM);
        obj.checksum_value = Some(crate::checksum::checksum(algorithm, &body));
        obj.checksum_algorithm = Some(algorithm);
        obj.checksum_type = Some(crate::checksum::ChecksumType::FullObject);
    }

    // Stage the payload with no S3 lock held: the global S3 lock is shared by
    // every bucket in every account, so a large export or log delivery written
    // to disk under it would stall all of S3.
    let mut spooled =
        crate::service::run_blocking_io(|| crate::body_io::SpooledBody::spool(store, body.clone()))
            .map_err(|e| format!("failed to stage object {key} for bucket {bucket}: {e}"))?;

    obj.body = memory_body(body);
    if obj.storage_class.is_empty() {
        obj.storage_class = "STANDARD".to_string();
    }
    let now = Utc::now();
    obj.last_modified = now;

    let mut accounts = state.write();
    let acct = accounts.get_or_create(account_id);
    let bucket_ref = acct.buckets.get_mut(bucket).ok_or_else(missing)?;
    // The bucket owner owns a delivered object, with the default private ACL.
    if obj.acl_owner_id.is_none() {
        obj.acl_owner_id = Some(bucket_ref.acl_owner_id.clone());
        obj.acl_grants = vec![AclGrant {
            grantee_type: "CanonicalUser".to_string(),
            grantee_id: Some(bucket_ref.acl_owner_id.clone()),
            grantee_display_name: Some(bucket_ref.acl_owner_id.clone()),
            grantee_uri: None,
            permission: "FULL_CONTROL".to_string(),
        }];
    }
    // Commit under the same guard as the existence check, so the on-disk body
    // and the in-memory metadata of concurrent writers to one key stay in
    // step. The payload is already staged next to the store, so the store
    // write is a rename, not a body copy.
    // A failed store write is returned to the caller with the bucket left
    // exactly as it was, so the caller's failure handling (a failed export
    // job, an error response) sees it and memory never claims an object the
    // disk lacks.
    crate::service::commit_object_version(bucket_ref, bucket, store, obj, spooled.source())
        .map_err(|e| format!("failed to persist object {key} to bucket {bucket}: {e}"))
}

impl S3Delivery for S3DeliveryImpl {
    fn put_object(
        &self,
        account_id: &str,
        bucket: &str,
        key: &str,
        body: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<(), String> {
        // Snapshot the durable store handle before taking the S3 state lock so
        // the two locks are never held nested.
        let store = self.store.read().clone();
        put_object_via_store(
            &self.state,
            store.as_deref(),
            account_id,
            bucket,
            key,
            body,
            content_type,
        )
    }

    fn get_object(&self, account_id: &str, bucket: &str, key: &str) -> Result<Vec<u8>, String> {
        // Resolve and open the body under the guard, read it after the guard
        // is dropped (the open fd pins the inode against a concurrent PUT).
        let handle = {
            let accounts = self.state.read();
            let state = accounts
                .get(account_id)
                .ok_or_else(|| format!("account {account_id} has no S3 state"))?;
            let bucket_ref = state
                .buckets
                .get(bucket)
                .ok_or_else(|| format!("bucket {bucket} not found in account {account_id}"))?;
            let object = bucket_ref
                .objects
                .get(key)
                .ok_or_else(|| format!("key {key} not found in bucket {bucket}"))?;
            crate::body_io::ObjectBodyHandle::open(&object.body)
                .map_err(|e| format!("failed to read body: {e}"))?
        };
        let body = crate::service::run_blocking_io(|| handle.read_all())
            .map_err(|e| format!("failed to read body: {e}"))?;
        Ok(body.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{S3Bucket, S3State};
    use fakecloud_core::multi_account::MultiAccountState;
    use parking_lot::RwLock;
    use std::sync::Arc;

    const ACCOUNT: &str = "123456789012";

    fn shared_state() -> SharedS3State {
        let mut mas: MultiAccountState<S3State> =
            MultiAccountState::new(ACCOUNT, "us-east-1", "http://localhost");
        let st = mas.get_or_create(ACCOUNT);
        st.buckets.insert(
            "logs-bucket".to_string(),
            S3Bucket::new("logs-bucket", "us-east-1", "owner-id"),
        );
        Arc::new(RwLock::new(mas))
    }

    #[test]
    fn put_object_writes_to_bucket() {
        let state = shared_state();
        let delivery = S3DeliveryImpl::new(state.clone());
        delivery
            .put_object(
                ACCOUNT,
                "logs-bucket",
                "exports/run-1.txt",
                b"hello world".to_vec(),
                Some("text/plain"),
            )
            .expect("put should succeed");
        let accounts = state.read();
        let st = accounts.get(ACCOUNT).unwrap();
        let bucket = st.buckets.get("logs-bucket").unwrap();
        let obj = bucket.objects.get("exports/run-1.txt").unwrap();
        assert_eq!(obj.size, 11);
        assert_eq!(obj.content_type, "text/plain");
        assert!(!obj.etag.is_empty());
    }

    #[test]
    fn put_object_rejects_missing_bucket() {
        let state = shared_state();
        let delivery = S3DeliveryImpl::new(state);
        let err = delivery
            .put_object(ACCOUNT, "ghost", "k", b"x".to_vec(), None)
            .expect_err("missing bucket");
        assert!(err.contains("not found"));
    }

    /// Regression (bug-hunt Tier 0 side-channel-persistence): a delivered object
    /// must be written through the durable S3 store, else it is lost on restart
    /// (S3 has no state snapshot; it is rebuilt from the store on boot). With a
    /// store wired, the in-memory body becomes the store's disk-backed ref and
    /// the object reappears after a store reload.
    #[test]
    fn put_object_writes_through_to_durable_store() {
        use fakecloud_persistence::s3::DiskS3Store;
        use fakecloud_persistence::{BodyRef, BucketMeta, S3Store};

        let tmp = tempfile::TempDir::new().unwrap();
        let cache = Arc::new(fakecloud_persistence::cache::BodyCache::new(1024 * 1024));
        let disk: Arc<dyn S3Store> = Arc::new(DiskS3Store::new(tmp.path().join("s3"), cache));
        disk.put_bucket_meta(
            "logs-bucket",
            &BucketMeta {
                name: "logs-bucket".to_string(),
                region: "us-east-1".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

        let state = shared_state();
        let delivery = S3DeliveryImpl::new(state.clone());
        delivery.set_s3_store(disk.clone());
        delivery
            .put_object(
                ACCOUNT,
                "logs-bucket",
                "exports/run-1.txt",
                b"durable-bytes".to_vec(),
                Some("text/plain"),
            )
            .expect("put");

        // In-memory body is now the store's disk-backed ref, and the file exists.
        {
            let accounts = state.read();
            let obj = accounts
                .get(ACCOUNT)
                .unwrap()
                .buckets
                .get("logs-bucket")
                .unwrap()
                .objects
                .get("exports/run-1.txt")
                .unwrap();
            match &obj.body {
                BodyRef::Disk { path, .. } => assert!(path.exists()),
                BodyRef::Memory(_) => panic!("delivery was not written through the store"),
            }
        }

        // Reloading the store from disk (as boot does) surfaces the object.
        let reloaded = disk.load().unwrap();
        assert!(reloaded
            .buckets
            .get("logs-bucket")
            .expect("bucket survives reload")
            .objects
            .contains_key("exports/run-1.txt"));
    }

    fn deliver(state: &SharedS3State, store: Option<&dyn S3Store>, key: &str, body: &[u8]) {
        put_object_via_store(
            state,
            store,
            ACCOUNT,
            "logs-bucket",
            key,
            body.to_vec(),
            Some("text/plain"),
        )
        .expect("delivery");
    }

    fn set_bucket(state: &SharedS3State, f: impl FnOnce(&mut S3Bucket)) {
        let mut accounts = state.write();
        f(accounts
            .get_mut(ACCOUNT)
            .unwrap()
            .buckets
            .get_mut("logs-bucket")
            .unwrap());
    }

    fn bucket(state: &SharedS3State) -> S3Bucket {
        state
            .read()
            .get(ACCOUNT)
            .unwrap()
            .buckets
            .get("logs-bucket")
            .unwrap()
            .clone()
    }

    fn read(state: &SharedS3State, body: &fakecloud_persistence::BodyRef) -> Vec<u8> {
        state
            .read()
            .get(ACCOUNT)
            .unwrap()
            .read_body(body)
            .unwrap()
            .to_vec()
    }

    #[test]
    fn delivery_to_unversioned_bucket_overwrites_without_history() {
        let state = shared_state();
        deliver(&state, None, "k", b"one");
        deliver(&state, None, "k", b"two");
        let b = bucket(&state);
        let obj = &b.objects["k"];
        assert_eq!(obj.version_id, None);
        assert_eq!(obj.storage_class, "STANDARD");
        assert_eq!(obj.acl_owner_id.as_deref(), Some("owner-id"));
        assert_eq!(read(&state, &obj.body), b"two");
        assert!(b.object_versions.is_empty());
        assert_eq!(obj.lock_mode, None);
    }

    /// A delivered object carries S3's default CRC64NVME full-object
    /// checksum of its body, like a PutObject that names no algorithm.
    #[test]
    fn delivery_stores_the_default_full_object_checksum() {
        use crate::checksum::{ChecksumAlgorithm, ChecksumType};
        let state = shared_state();
        deliver(&state, None, "k", b"payload");
        let obj = &bucket(&state).objects["k"];
        assert_eq!(obj.checksum_algorithm, Some(ChecksumAlgorithm::Crc64Nvme));
        assert_eq!(obj.checksum_type, Some(ChecksumType::FullObject));
        assert_eq!(
            obj.checksum_value.as_deref(),
            Some(crate::checksum::checksum(ChecksumAlgorithm::Crc64Nvme, b"payload").as_str())
        );
        assert_eq!(obj.part_checksums, None);
    }

    /// A producer-supplied checksum (one over the plaintext of an encrypted
    /// payload) is kept, and an encrypted body is never hashed for a default.
    #[test]
    fn delivery_keeps_a_supplied_checksum_and_skips_encrypted_bodies() {
        use crate::checksum::{ChecksumAlgorithm, ChecksumType};
        let state = shared_state();
        let supplied = S3Object {
            key: "with".to_string(),
            sse_algorithm: Some("aws:kms".to_string()),
            checksum_algorithm: Some(ChecksumAlgorithm::Crc64Nvme),
            checksum_value: Some("plaintext-sum".to_string()),
            checksum_type: Some(ChecksumType::FullObject),
            ..Default::default()
        };
        put_delivered_object(
            &state,
            None,
            ACCOUNT,
            "logs-bucket",
            supplied,
            Bytes::from_static(b"cipher"),
        )
        .unwrap();
        let encrypted = S3Object {
            key: "without".to_string(),
            sse_algorithm: Some("aws:kms".to_string()),
            ..Default::default()
        };
        put_delivered_object(
            &state,
            None,
            ACCOUNT,
            "logs-bucket",
            encrypted,
            Bytes::from_static(b"cipher"),
        )
        .unwrap();
        let b = bucket(&state);
        assert_eq!(
            b.objects["with"].checksum_value.as_deref(),
            Some("plaintext-sum")
        );
        assert_eq!(b.objects["without"].checksum_value, None);
        assert_eq!(b.objects["without"].checksum_algorithm, None);
    }

    #[test]
    fn delivery_to_versioned_bucket_adds_a_version_each_write() {
        let state = shared_state();
        // Written before versioning: becomes the "null" version.
        deliver(&state, None, "k", b"pre");
        set_bucket(&state, |b| b.versioning = Some("Enabled".to_string()));
        deliver(&state, None, "k", b"one");
        deliver(&state, None, "k", b"two");

        let b = bucket(&state);
        let versions = &b.object_versions["k"];
        assert_eq!(versions.len(), 3, "{versions:?}");
        assert_eq!(versions[0].version_id.as_deref(), Some("null"));
        assert_eq!(read(&state, &versions[0].body), b"pre");
        let v1 = versions[1].version_id.clone().unwrap();
        let v2 = versions[2].version_id.clone().unwrap();
        assert_ne!(v1, "null");
        assert_ne!(v1, v2);
        assert_eq!(read(&state, &versions[1].body), b"one");
        assert_eq!(read(&state, &versions[2].body), b"two");
        let current = &b.objects["k"];
        assert_eq!(current.version_id.as_deref(), Some(v2.as_str()));
        assert_eq!(read(&state, &current.body), b"two");
    }

    #[test]
    fn delivery_reports_the_new_version_id() {
        let state = shared_state();
        set_bucket(&state, |b| b.versioning = Some("Enabled".to_string()));
        let obj = S3Object {
            key: "k".to_string(),
            ..Default::default()
        };
        let vid = put_delivered_object(&state, None, ACCOUNT, "logs-bucket", obj, Bytes::new())
            .unwrap()
            .expect("a version id on a versioned bucket");
        assert_eq!(
            bucket(&state).objects["k"].version_id.as_deref(),
            Some(vid.as_str())
        );
    }

    #[test]
    fn delivery_to_suspended_bucket_takes_over_the_null_version() {
        let state = shared_state();
        set_bucket(&state, |b| b.versioning = Some("Enabled".to_string()));
        deliver(&state, None, "k", b"versioned");
        set_bucket(&state, |b| b.versioning = Some("Suspended".to_string()));
        deliver(&state, None, "k", b"null-1");
        deliver(&state, None, "k", b"null-2");

        let b = bucket(&state);
        let current = &b.objects["k"];
        assert_eq!(current.version_id.as_deref(), Some("null"));
        assert_eq!(read(&state, &current.body), b"null-2");
        let versions = &b.object_versions["k"];
        // The enabled-era version survives; the null slot holds only the
        // latest suspended write.
        assert_eq!(versions.len(), 2, "{versions:?}");
        assert_ne!(versions[0].version_id.as_deref(), Some("null"));
        assert_eq!(read(&state, &versions[0].body), b"versioned");
        assert_eq!(versions[1].version_id.as_deref(), Some("null"));
        assert_eq!(read(&state, &versions[1].body), b"null-2");
    }

    #[test]
    fn delivery_to_lock_bucket_applies_default_retention() {
        let state = shared_state();
        set_bucket(&state, |b| {
            b.versioning = Some("Enabled".to_string());
            b.object_lock_config = Some(
                "<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled>\
                 <Rule><DefaultRetention><Mode>GOVERNANCE</Mode><Days> 3 </Days>\
                 </DefaultRetention></Rule></ObjectLockConfiguration>"
                    .to_string(),
            );
        });
        let before = Utc::now();
        deliver(&state, None, "k", b"locked");
        let after = Utc::now();

        let b = bucket(&state);
        let obj = &b.objects["k"];
        assert_eq!(obj.lock_mode.as_deref(), Some("GOVERNANCE"));
        let until = obj.lock_retain_until.expect("retain-until");
        assert!(until >= before + chrono::Duration::days(3));
        assert!(until <= after + chrono::Duration::days(3));
        assert_eq!(obj.lock_legal_hold, None);
        // The version entry carries the same lock.
        let version = b.object_versions["k"].last().unwrap();
        assert_eq!(version.lock_mode.as_deref(), Some("GOVERNANCE"));
        assert_eq!(version.lock_retain_until, Some(until));
    }

    #[test]
    fn delivery_default_retention_in_years_uses_calendar_years() {
        let state = shared_state();
        set_bucket(&state, |b| {
            b.versioning = Some("Enabled".to_string());
            b.object_lock_config = Some(
                "<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled>\
                 <Rule><DefaultRetention><Mode>COMPLIANCE</Mode><Years>1</Years>\
                 </DefaultRetention></Rule></ObjectLockConfiguration>"
                    .to_string(),
            );
        });
        let before = Utc::now();
        deliver(&state, None, "k", b"locked");
        let after = Utc::now();
        let obj = &bucket(&state).objects["k"];
        assert_eq!(obj.lock_mode.as_deref(), Some("COMPLIANCE"));
        let until = obj.lock_retain_until.unwrap();
        assert!(until >= before.checked_add_months(chrono::Months::new(12)).unwrap());
        assert!(until <= after.checked_add_months(chrono::Months::new(12)).unwrap());
    }

    #[test]
    fn delivery_keeps_an_explicit_lock_over_the_bucket_default() {
        let state = shared_state();
        set_bucket(&state, |b| {
            b.versioning = Some("Enabled".to_string());
            b.object_lock_config = Some(
                "<ObjectLockConfiguration><Rule><DefaultRetention><Mode>GOVERNANCE</Mode>\
                 <Days>1</Days></DefaultRetention></Rule></ObjectLockConfiguration>"
                    .to_string(),
            );
        });
        let until = Utc::now() + chrono::Duration::days(30);
        let obj = S3Object {
            key: "k".to_string(),
            lock_mode: Some("COMPLIANCE".to_string()),
            lock_retain_until: Some(until),
            ..Default::default()
        };
        put_delivered_object(&state, None, ACCOUNT, "logs-bucket", obj, Bytes::new()).unwrap();
        let obj = &bucket(&state).objects["k"];
        assert_eq!(obj.lock_mode.as_deref(), Some("COMPLIANCE"));
        assert_eq!(obj.lock_retain_until, Some(until));
    }

    /// Versioned deliveries persist like PutObject's: every version (and the
    /// preserved pre-versioning "null" one) survives a reload of the store,
    /// with its own body and lock.
    #[test]
    fn versioned_delivery_persists_every_version() {
        use fakecloud_persistence::s3::DiskS3Store;
        use fakecloud_persistence::BucketMeta;

        let tmp = tempfile::TempDir::new().unwrap();
        let cache = Arc::new(fakecloud_persistence::cache::BodyCache::new(1024 * 1024));
        let disk: Arc<dyn S3Store> = Arc::new(DiskS3Store::new(tmp.path().join("s3"), cache));
        disk.put_bucket_meta(
            "logs-bucket",
            &BucketMeta {
                name: "logs-bucket".to_string(),
                region: "us-east-1".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

        let state = shared_state();
        deliver(&state, Some(disk.as_ref()), "k", b"pre");
        set_bucket(&state, |b| {
            b.versioning = Some("Enabled".to_string());
            b.object_lock_config = Some(
                "<ObjectLockConfiguration><Rule><DefaultRetention><Mode>GOVERNANCE</Mode>\
                 <Days>1</Days></DefaultRetention></Rule></ObjectLockConfiguration>"
                    .to_string(),
            );
        });
        deliver(&state, Some(disk.as_ref()), "k", b"one");
        deliver(&state, Some(disk.as_ref()), "k", b"two");
        let live = bucket(&state);

        let reloaded = disk.load().unwrap();
        let snap = reloaded.buckets.get("logs-bucket").unwrap();
        let mut ids: Vec<Option<String>> = snap.object_versions["k"]
            .iter()
            .map(|o| o.meta.version_id.clone())
            .collect();
        let mut live_ids: Vec<Option<String>> = live.object_versions["k"]
            .iter()
            .map(|o| o.version_id.clone())
            .collect();
        ids.sort();
        live_ids.sort();
        assert_eq!(ids, live_ids);
        let current = &snap.objects["k"];
        assert_eq!(current.meta.version_id, live.objects["k"].version_id);
        assert_eq!(current.meta.lock_mode.as_deref(), Some("GOVERNANCE"));
        match &current.body {
            fakecloud_persistence::BodyRef::Disk { path, .. } => {
                assert_eq!(std::fs::read(path).unwrap(), b"two");
            }
            fakecloud_persistence::BodyRef::Memory(_) => panic!("expected a disk body"),
        }
    }

    /// A store whose object writes always fail, standing in for a disk error.
    struct FailingStore {
        inner: fakecloud_persistence::MemoryS3Store,
        fail_meta: bool,
    }

    fn disk_full() -> fakecloud_persistence::StoreError {
        fakecloud_persistence::StoreError::Other("disk full".to_string())
    }

    impl S3Store for FailingStore {
        fn load(
            &self,
        ) -> fakecloud_persistence::StoreResult<fakecloud_persistence::S3StateSnapshot> {
            self.inner.load()
        }
        fn put_bucket_meta(
            &self,
            bucket: &str,
            meta: &fakecloud_persistence::BucketMeta,
        ) -> fakecloud_persistence::StoreResult<()> {
            self.inner.put_bucket_meta(bucket, meta)
        }
        fn put_bucket_subresource(
            &self,
            bucket: &str,
            kind: fakecloud_persistence::BucketSubresource,
            payload: &str,
        ) -> fakecloud_persistence::StoreResult<()> {
            self.inner.put_bucket_subresource(bucket, kind, payload)
        }
        fn delete_bucket_subresource(
            &self,
            bucket: &str,
            kind: fakecloud_persistence::BucketSubresource,
        ) -> fakecloud_persistence::StoreResult<()> {
            self.inner.delete_bucket_subresource(bucket, kind)
        }
        fn delete_bucket(&self, bucket: &str) -> fakecloud_persistence::StoreResult<()> {
            self.inner.delete_bucket(bucket)
        }
        fn put_object(
            &self,
            _bucket: &str,
            _key: &str,
            _version: Option<&str>,
            _body: fakecloud_persistence::BodySource,
            _meta: &fakecloud_persistence::ObjectMeta,
        ) -> fakecloud_persistence::StoreResult<fakecloud_persistence::BodyRef> {
            Err(disk_full())
        }
        fn put_object_meta(
            &self,
            bucket: &str,
            key: &str,
            version: Option<&str>,
            meta: &fakecloud_persistence::ObjectMeta,
        ) -> fakecloud_persistence::StoreResult<()> {
            if self.fail_meta {
                return Err(disk_full());
            }
            self.inner.put_object_meta(bucket, key, version, meta)
        }
        fn delete_object(
            &self,
            bucket: &str,
            key: &str,
            version: Option<&str>,
        ) -> fakecloud_persistence::StoreResult<()> {
            self.inner.delete_object(bucket, key, version)
        }
        fn open_object_body(
            &self,
            body: &fakecloud_persistence::BodyRef,
        ) -> fakecloud_persistence::StoreResult<Bytes> {
            self.inner.open_object_body(body)
        }
        fn mpu_create(
            &self,
            bucket: &str,
            upload_id: &str,
            init: &fakecloud_persistence::MpuInit,
        ) -> fakecloud_persistence::StoreResult<()> {
            self.inner.mpu_create(bucket, upload_id, init)
        }
        fn mpu_put_part(
            &self,
            bucket: &str,
            upload_id: &str,
            part_number: u32,
            body: fakecloud_persistence::BodySource,
            etag: &str,
            checksum: Option<fakecloud_persistence::PartChecksum<'_>>,
        ) -> fakecloud_persistence::StoreResult<fakecloud_persistence::BodyRef> {
            self.inner
                .mpu_put_part(bucket, upload_id, part_number, body, etag, checksum)
        }
        fn mpu_abort(
            &self,
            bucket: &str,
            upload_id: &str,
        ) -> fakecloud_persistence::StoreResult<()> {
            self.inner.mpu_abort(bucket, upload_id)
        }
        fn mpu_complete(
            &self,
            bucket: &str,
            upload_id: &str,
            final_key: &str,
            version: Option<&str>,
            meta: &fakecloud_persistence::ObjectMeta,
        ) -> fakecloud_persistence::StoreResult<fakecloud_persistence::BodyRef> {
            self.inner
                .mpu_complete(bucket, upload_id, final_key, version, meta)
        }
    }

    /// A failed store write is returned to the caller and records no new
    /// object or version in memory, and memory keeps agreeing with disk: when
    /// only the body write fails, the pre-versioning object's sidecar was
    /// already relabelled the "null" version, so memory records it as the null
    /// version too; when that relabel itself fails, nothing changes.
    #[test]
    fn store_failure_is_returned_and_leaves_memory_unchanged() {
        for fail_meta in [false, true] {
            let store = FailingStore {
                inner: fakecloud_persistence::MemoryS3Store::new(),
                fail_meta,
            };
            let state = shared_state();
            // An unversioned write that fails stores nothing.
            let err = put_object_via_store(
                &state,
                Some(&store),
                ACCOUNT,
                "logs-bucket",
                "k",
                b"lost".to_vec(),
                None,
            )
            .expect_err("store failure must be returned");
            assert!(err.contains("disk full"), "{err}");
            assert!(bucket(&state).objects.is_empty());

            // A versioned write over a pre-versioning object leaves both the
            // current object and the (empty) history as they were.
            deliver(&state, None, "k", b"pre");
            set_bucket(&state, |b| b.versioning = Some("Enabled".to_string()));
            let before = bucket(&state);
            put_object_via_store(
                &state,
                Some(&store),
                ACCOUNT,
                "logs-bucket",
                "k",
                b"one".to_vec(),
                None,
            )
            .expect_err("store failure must be returned");
            let after = bucket(&state);
            let current = &after.objects["k"];
            assert_eq!(read(&state, &current.body), b"pre");
            assert_eq!(before.objects["k"].version_id, None);
            if fail_meta {
                // The relabel failed: disk and memory both still hold the
                // untagged pre-versioning object.
                assert!(after.object_versions.is_empty());
                assert_eq!(current.version_id, None);
            } else {
                // The relabel reached disk, so memory records the object as
                // the null version, and no new version exists.
                assert_eq!(current.version_id.as_deref(), Some("null"));
                let versions = &after.object_versions["k"];
                assert_eq!(versions.len(), 1, "{versions:?}");
                assert_eq!(versions[0].version_id.as_deref(), Some("null"));
                assert_eq!(read(&state, &versions[0].body), b"pre");
            }
        }
    }

    #[test]
    fn get_object_round_trips_put_object() {
        let state = shared_state();
        let delivery = S3DeliveryImpl::new(state);
        delivery
            .put_object(
                ACCOUNT,
                "logs-bucket",
                "dump.sql",
                b"-- pg dump --".to_vec(),
                Some("application/sql"),
            )
            .expect("put");
        let body = delivery
            .get_object(ACCOUNT, "logs-bucket", "dump.sql")
            .expect("get");
        assert_eq!(body, b"-- pg dump --".to_vec());
    }

    #[test]
    fn get_object_rejects_missing_key() {
        let state = shared_state();
        let delivery = S3DeliveryImpl::new(state);
        let err = delivery
            .get_object(ACCOUNT, "logs-bucket", "ghost")
            .expect_err("missing key");
        assert!(err.contains("not found"));
    }
}
