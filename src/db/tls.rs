#![allow(dead_code)] // Consumed by the Milestone 2 session connector.

use crate::error::ApplicationError;
use rustls::{
    ClientConfig, RootCertStore,
    crypto::aws_lc_rs,
    pki_types::{CertificateDer, pem::PemObject},
};
use std::{io::Read, sync::Arc};
use tokio_postgres_rustls::MakeRustlsConnect;

const MAX_ROOT_CERTIFICATE_FILE_BYTES: u64 = 1024 * 1024;
const MAX_ROOT_CERTIFICATE_FILES: usize = 16;
const MAX_ADDITIONAL_ROOT_CERTIFICATES: usize = 128;

/// Build the TLS connector used for PostgreSQL connections.
///
/// The bundled Mozilla roots remain trusted when callers add PEM roots. This
/// is intentionally a normal rustls verifier: it verifies both the server
/// certificate chain and the server name supplied by tokio-postgres.
pub(crate) fn make_rustls_connect(
    additional_root_paths: &[String],
) -> Result<MakeRustlsConnect, ApplicationError> {
    Ok(MakeRustlsConnect::new(client_config(
        additional_root_paths,
    )?))
}

fn client_config(additional_root_paths: &[String]) -> Result<ClientConfig, ApplicationError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    add_pem_roots(&mut roots, additional_root_paths)?;

    let config = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|_| ApplicationError::runtime("could not configure TLS protocol versions"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(config)
}

fn add_pem_roots(
    roots: &mut RootCertStore,
    additional_root_paths: &[String],
) -> Result<(), ApplicationError> {
    if additional_root_paths.len() > MAX_ROOT_CERTIFICATE_FILES {
        return Err(ApplicationError::runtime(
            "too many TLS root certificate files",
        ));
    }
    let mut total_certificates = 0;
    for path in additional_root_paths {
        let file = std::fs::File::open(path)
            .map_err(|_| ApplicationError::runtime("could not read TLS root certificate file"))?;
        let metadata = file.metadata().map_err(|_| {
            ApplicationError::runtime("could not inspect TLS root certificate file")
        })?;
        if !metadata.is_file() {
            return Err(ApplicationError::runtime(
                "TLS root certificate path must be a regular file",
            ));
        }
        if metadata.len() > MAX_ROOT_CERTIFICATE_FILE_BYTES {
            return Err(ApplicationError::runtime(
                "TLS root certificate file is too large",
            ));
        }

        let mut pem = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_ROOT_CERTIFICATE_FILE_BYTES + 1)
            .read_to_end(&mut pem)
            .map_err(|_| ApplicationError::runtime("could not read TLS root certificate file"))?;
        if pem.len() as u64 > MAX_ROOT_CERTIFICATE_FILE_BYTES {
            return Err(ApplicationError::runtime(
                "TLS root certificate file is too large",
            ));
        }

        let mut count = 0;
        for certificate in CertificateDer::pem_slice_iter(&pem) {
            let certificate = certificate.map_err(|_| {
                ApplicationError::runtime("TLS root certificate file contains malformed PEM")
            })?;
            roots.add(certificate).map_err(|_| {
                ApplicationError::runtime(
                    "TLS root certificate file contains an invalid certificate",
                )
            })?;
            count += 1;
            total_certificates += 1;
            if total_certificates > MAX_ADDITIONAL_ROOT_CERTIFICATES {
                return Err(ApplicationError::runtime("too many TLS root certificates"));
            }
        }

        if count == 0 {
            return Err(ApplicationError::runtime(
                "TLS root certificate file contains no certificates",
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{client_config, make_rustls_connect};
    use rcgen::generate_simple_self_signed;
    use rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer},
    };
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tokio::net::TcpListener;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    static TEMPORARY_FILE_ID: AtomicUsize = AtomicUsize::new(0);

    struct TemporaryPem(PathBuf);

    impl TemporaryPem {
        fn new(contents: &str) -> Self {
            let id = TEMPORARY_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "dsql-cli-tls-test-{}-{}-{id}.pem",
                std::process::id(),
                std::thread::current().name().unwrap_or("test"),
            ));
            fs::write(&path, contents).expect("write test PEM");
            Self(path)
        }

        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for TemporaryPem {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn server_config(hostname: &str) -> (ServerConfig, String) {
        let certified_key = generate_simple_self_signed(vec![hostname.into()])
            .expect("generate self-signed certificate");
        let certificate = certified_key.cert;
        let private_key = certified_key.signing_key;
        let pem = certificate.pem();
        let config = ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .expect("supported protocol versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(certificate.der().to_vec())],
            PrivateKeyDer::Pkcs8(private_key.serialize_der().into()),
        )
        .expect("valid test certificate");
        (config, pem)
    }

    async fn serve_once(config: ServerConfig) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let address = listener.local_addr().expect("server address");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let _ = TlsAcceptor::from(Arc::new(config)).accept(stream).await;
        });
        address
    }

    async fn connect(
        config: ServerConfig,
        roots: &[String],
        hostname: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let address = serve_once(config).await;
        let stream = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect TCP");
        let name = rustls::pki_types::ServerName::try_from(hostname)
            .expect("valid server name")
            .to_owned();
        TlsConnector::from(Arc::new(client_config(roots)?))
            .connect(name, stream)
            .await
            .map(|_| ())
            .map_err(Into::into)
    }

    #[test]
    fn builds_a_postgres_rustls_connector_with_bundled_roots() {
        make_rustls_connect(&[]).expect("bundled roots build a connector");
    }

    #[tokio::test]
    async fn additional_pem_roots_verify_a_local_certificate_chain() {
        let (config, trusted_pem) = server_config("localhost");
        let unrelated = TemporaryPem::new(&server_config("unrelated.test").1);
        let trusted = TemporaryPem::new(&trusted_pem);

        connect(config, &[unrelated.path(), trusted.path()], "localhost")
            .await
            .expect("the additive root trusts the local server");
    }

    #[tokio::test]
    async fn wrong_hostname_is_rejected_even_when_the_ca_is_trusted() {
        let (config, trusted_pem) = server_config("localhost");
        let trusted = TemporaryPem::new(&trusted_pem);

        assert!(
            connect(config, &[trusted.path()], "wrong-host.test")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unknown_ca_is_rejected() {
        let (config, _) = server_config("localhost");

        assert!(connect(config, &[], "localhost").await.is_err());
    }

    #[test]
    fn malformed_pem_is_a_safe_application_error() {
        let pem = TemporaryPem::new(
            "-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----\n",
        );
        let Err(error) = make_rustls_connect(&[pem.path()]) else {
            panic!("malformed PEM is rejected");
        };

        assert!(error.to_string().contains("malformed PEM"));
        assert!(!error.to_string().contains("not base64"));
        assert!(
            !error
                .to_string()
                .contains(Path::new(&pem.path()).to_string_lossy().as_ref())
        );
    }

    #[test]
    fn empty_and_invalid_certificate_files_are_rejected() {
        let empty = TemporaryPem::new("\n\t");
        let invalid =
            TemporaryPem::new("-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n");

        assert!(make_rustls_connect(&[empty.path()]).is_err());
        assert!(make_rustls_connect(&[invalid.path()]).is_err());
    }

    #[test]
    fn directories_and_oversized_root_files_are_rejected() {
        let directory = std::env::temp_dir();
        let oversized =
            TemporaryPem::new(&"x".repeat(super::MAX_ROOT_CERTIFICATE_FILE_BYTES as usize + 1));

        let directory_error = make_rustls_connect(&[directory.to_string_lossy().into_owned()])
            .err()
            .expect("directory is rejected");
        let oversized_error = make_rustls_connect(&[oversized.path()])
            .err()
            .expect("oversized file is rejected");

        assert!(directory_error.to_string().contains("regular file"));
        assert!(oversized_error.to_string().contains("too large"));
    }

    #[test]
    fn additional_root_file_and_certificate_counts_are_bounded() {
        let paths = vec!["unused".to_owned(); super::MAX_ROOT_CERTIFICATE_FILES + 1];
        assert_eq!(
            client_config(&paths).unwrap_err().to_string(),
            "too many TLS root certificate files"
        );

        let (_, pem) = server_config("database.internal");
        let roots = TemporaryPem::new(&pem.repeat(super::MAX_ADDITIONAL_ROOT_CERTIFICATES + 1));
        assert_eq!(
            client_config(&[roots.path()]).unwrap_err().to_string(),
            "too many TLS root certificates"
        );
    }
}
