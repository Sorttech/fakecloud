//! TLS for the PostgreSQL containers behind `AWS::RDS::DBInstance`.
//!
//! RDS for PostgreSQL "uses and expects all clients to connect using SSL/TLS", and `rds.force_ssl`
//! defaults to 1 from version 15 (<https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.SSL.md>,
//! fetched 2026-10-20). So the emulator mints one CA per process, gives each container a server
//! certificate naming the address clients dial, and publishes the CA so a verifying client
//! (`sslmode=verify-full`, `sslrootcert=...`) can trust it.
//!
//! The published image needs no change: copying a certificate and key in between `create` and
//! `start` and starting `postgres` with `-c ssl=on ...` is enough (probed 2026-10-20 against
//! `fakecloud-postgres:16-0.50.0`: `show ssl` = `on`, `pg_stat_ssl.ssl` = `t`).

use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};

/// Environment variable naming the file the CA certificate is written to at startup, so a client
/// outside fakecloud can pass it as `sslrootcert`. Unset: the CA is still minted and used, but not
/// published.
pub const CA_FILE_ENV: &str = "FAKECLOUD_RDS_CA_FILE";

/// The user id of `postgres` in the official Debian-based images. `postgres` refuses a key file it
/// does not own (or that is group/world readable), so the files are copied in owned by this id.
const POSTGRES_UID: u64 = 999;

/// Where the files land inside the container.
pub const CONTAINER_TLS_DIR: &str = "/tls";

/// The arguments that make `postgres` serve TLS from [`CONTAINER_TLS_DIR`] and read the
/// connection rules from the copied `pg_hba.conf` rather than the one in the data directory
/// (which the image's entrypoint writes with a plain `host` rule, and which would outlive a change
/// of `rds.force_ssl` inside the data volume).
pub const POSTGRES_TLS_ARGS: [&str; 8] = [
    "-c",
    "ssl=on",
    "-c",
    "ssl_cert_file=/tls/server.crt",
    "-c",
    "ssl_key_file=/tls/server.key",
    "-c",
    "hba_file=/tls/pg_hba.conf",
];

pub struct RdsCa {
    cert: rcgen::Certificate,
    key: KeyPair,
}

/// The process's CA, minted on first use. Written to [`CA_FILE_ENV`] when that is set.
///
/// ponytail: one CA per process, not persisted. Server certificates are re-minted whenever a
/// container is created or recovered after a restart, so they always chain to the live CA, but a
/// client that cached the CA file across a fakecloud restart must read it again. Persisting it
/// under the data directory is the upgrade if that matters.
pub fn ca() -> &'static RdsCa {
    static CA: OnceLock<RdsCa> = OnceLock::new();
    CA.get_or_init(|| {
        let ca = RdsCa::generate().expect("generate the RDS CA");
        if let Ok(path) = std::env::var(CA_FILE_ENV) {
            if !path.is_empty() {
                if let Err(error) = ca.publish(Path::new(&path)) {
                    tracing::warn!(%error, path, "could not write the RDS CA file");
                }
            }
        }
        ca
    })
}

impl RdsCa {
    fn generate() -> Result<Self, rcgen::Error> {
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params
            .distinguished_name
            .push(DnType::CommonName, "fakecloud RDS CA");
        params
            .distinguished_name
            .push(DnType::OrganizationName, "fakecloud");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let key = KeyPair::generate()?;
        let cert = params.self_signed(&key)?;
        Ok(Self { cert, key })
    }

    pub fn cert_pem(&self) -> String {
        self.cert.pem()
    }

    fn publish(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.cert_pem())
    }

    /// A server certificate for `hosts` (the addresses clients dial), as `(cert_pem, key_pem)`.
    /// The first host is the Common Name, as on RDS where it is the instance endpoint; every host
    /// is also a subject alternative name, which is what a verifying client actually checks.
    pub fn server_certificate(&self, hosts: &[String]) -> Result<(String, String), rcgen::Error> {
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        if let Some(first) = hosts.first() {
            params
                .distinguished_name
                .push(DnType::CommonName, first.as_str());
        }
        for host in hosts {
            params.subject_alt_names.push(match host.parse::<IpAddr>() {
                Ok(ip) => SanType::IpAddress(ip),
                Err(_) => SanType::DnsName(host.as_str().try_into()?),
            });
        }
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.cert, &self.key)?;
        Ok((cert.pem(), key.serialize_pem()))
    }
}

/// The connection rules for a container. The local lines are the image's own; the last is what
/// differs: with `rds.force_ssl` on, only `hostssl` matches, so a plain connection is refused with
/// `no pg_hba.conf entry for host "...", user "...", database "..."` (the engine's own error, the
/// one RDS gives for `rds.force_ssl`; its suffix is `no encryption` on PostgreSQL 16).
pub fn pg_hba_conf(force_ssl: bool) -> String {
    let remote = if force_ssl { "hostssl" } else { "host" };
    format!(
        "local   all             all                                     trust\n\
         host    all             all             127.0.0.1/32            trust\n\
         host    all             all             ::1/128                 trust\n\
         local   replication     all                                     trust\n\
         host    replication     all             127.0.0.1/32            trust\n\
         host    replication     all             ::1/128                 trust\n\
         {remote} all            all             all                     scram-sha-256\n"
    )
}

/// A tar stream that creates [`CONTAINER_TLS_DIR`] with the certificate, key and `pg_hba.conf`,
/// owned by `postgres`. Fed to `docker cp - <container>:/`, which (unlike a copy from a host
/// directory, where everything becomes root's) keeps the ownership the archive carries.
pub fn tls_tar(cert_pem: &str, key_pem: &str, force_ssl: bool) -> std::io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut dir = tar::Header::new_gnu();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_path("tls/")?;
    dir.set_mode(0o755);
    dir.set_uid(POSTGRES_UID);
    dir.set_gid(POSTGRES_UID);
    dir.set_size(0);
    dir.set_cksum();
    builder.append(&dir, std::io::empty())?;
    for (name, mode, content) in [
        ("tls/server.crt", 0o644, cert_pem.to_string()),
        ("tls/server.key", 0o600, key_pem.to_string()),
        ("tls/pg_hba.conf", 0o644, pg_hba_conf(force_ssl)),
    ] {
        append_file(&mut builder, name, mode, content.as_bytes())?;
    }
    builder.into_inner()
}

/// A tar stream holding only `pg_hba.conf`, for the reboot that applies a changed `rds.force_ssl`.
pub fn hba_tar(force_ssl: bool) -> std::io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    append_file(
        &mut builder,
        "tls/pg_hba.conf",
        0o644,
        pg_hba_conf(force_ssl).as_bytes(),
    )?;
    builder.into_inner()
}

fn append_file(
    builder: &mut tar::Builder<Vec<u8>>,
    name: &str,
    mode: u32,
    content: &[u8],
) -> std::io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_path(name)?;
    header.set_mode(mode);
    header.set_uid(POSTGRES_UID);
    header.set_gid(POSTGRES_UID);
    header.set_size(content.len() as u64);
    header.set_cksum();
    builder.append(&header, content)
}

/// A client config that trusts this process's CA, for fakecloud's own connections to the
/// containers (readiness, RDS Data API). Used with `sslmode=prefer`: TLS when the server offers it,
/// plain when it does not (the Kubernetes backend's pods).
pub fn client_connector() -> tokio_postgres_rustls::MakeRustlsConnect {
    tokio_postgres_rustls::MakeRustlsConnect::new(client_config(ca()))
}

fn client_config(ca: &RdsCa) -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(ca.cert.der().clone())
        .expect("the RDS CA is a valid trust anchor");
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::client::WebPkiServerVerifier;
    use rustls::pki_types::{ServerName, UnixTime};
    use std::io::Read;

    fn verify(ca: &RdsCa, cert_pem: &str, host: &str) -> Result<(), rustls::Error> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.cert.der().clone()).unwrap();
        let verifier = WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap();
        let leaf = rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes()).unwrap();
        verifier
            .verify_server_cert(
                &leaf,
                &[],
                &ServerName::try_from(host.to_string()).unwrap(),
                &[],
                UnixTime::now(),
            )
            .map(|_| ())
    }

    use rustls::pki_types::pem::PemObject;

    #[test]
    fn server_certificate_chains_to_the_ca_and_names_every_host() {
        let ca = RdsCa::generate().unwrap();
        let hosts = vec![
            "127.0.0.1".to_string(),
            "host.docker.internal".to_string(),
            "localhost".to_string(),
        ];
        let (cert_pem, key_pem) = ca.server_certificate(&hosts).unwrap();
        assert!(key_pem.contains("PRIVATE KEY"));
        for host in &hosts {
            verify(&ca, &cert_pem, host).unwrap_or_else(|e| panic!("{host}: {e}"));
        }
    }

    #[test]
    fn server_certificate_is_rejected_for_a_host_it_does_not_name() {
        let ca = RdsCa::generate().unwrap();
        let (cert_pem, _) = ca.server_certificate(&["127.0.0.1".to_string()]).unwrap();
        assert!(verify(&ca, &cert_pem, "evil.example.com").is_err());
    }

    #[test]
    fn server_certificate_is_rejected_by_another_ca() {
        let ca = RdsCa::generate().unwrap();
        let other = RdsCa::generate().unwrap();
        let (cert_pem, _) = ca.server_certificate(&["127.0.0.1".to_string()]).unwrap();
        assert!(verify(&other, &cert_pem, "127.0.0.1").is_err());
    }

    #[test]
    fn force_ssl_decides_whether_a_plain_remote_connection_matches() {
        let forced = pg_hba_conf(true);
        let relaxed = pg_hba_conf(false);
        assert!(forced.contains("hostssl all"));
        assert!(!forced.contains("\nhost    all             all             all"));
        assert!(relaxed.contains("host all"));
        assert!(!relaxed.contains("hostssl"));
        // The image's own local rules stay either way: fakecloud's `docker exec` clients use them.
        for text in [&forced, &relaxed] {
            assert!(text
                .contains("local   all             all                                     trust"));
        }
    }

    #[test]
    fn tls_tar_carries_postgres_owned_files_with_a_private_key() {
        let bytes = tls_tar("CERT", "KEY", true).unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        let mut seen = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            assert_eq!(entry.header().uid().unwrap(), POSTGRES_UID, "{path}");
            let mode = entry.header().mode().unwrap();
            let mut body = String::new();
            entry.read_to_string(&mut body).unwrap();
            seen.push((path, mode, body));
        }
        let key = seen.iter().find(|(p, ..)| p == "tls/server.key").unwrap();
        assert_eq!((key.1, key.2.as_str()), (0o600, "KEY"));
        let hba = seen.iter().find(|(p, ..)| p == "tls/pg_hba.conf").unwrap();
        assert!(hba.2.contains("hostssl"));
        assert!(seen
            .iter()
            .any(|(p, _, b)| p == "tls/server.crt" && b == "CERT"));
    }

    #[test]
    fn the_ca_file_is_written_where_asked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("ca.pem");
        let ca = RdsCa::generate().unwrap();
        ca.publish(&path).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), ca.cert_pem());
    }
}
