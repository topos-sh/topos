//! The optional built-in ACME TLS serve path — BIN-SIDE composition only, behind the default-off `acme`
//! cargo feature (this module hangs off `main.rs`; the `topos-plane` LIBRARY surface gains nothing). It is
//! **experimental**: the recommended TLS posture stays "terminate at a reverse proxy" — this path exists
//! for the single-box self-host that wants the plane to hold its own certificate.
//!
//! Mechanics: `rustls-acme` (ring-only — no second crypto backend enters the graph) drives the ACME order
//! against the configured directory and answers the **tls-alpn-01** challenge INSIDE the TLS acceptor, so
//! certificate issuance and regular serving share one port (the operator maps public 443 to `--acme-bind`).
//! The plain-HTTP listener keeps serving unchanged beside it (healthchecks, loopback, the reverse-proxy
//! lane). The ACME account + issued certificates persist under `--acme-cache`, so a restart re-serves the
//! cached certificate without re-contacting the ACME server.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use rustls_acme::caches::DirCache;
use rustls_acme::{AcmeConfig, rustls};
use topos_plane::{PlaneState, router};

use crate::Config;

/// The resolved, validated ACME settings. `Some` only when the operator turned the listener on — a
/// non-empty `--acme-domain` / `TOPOS_PLANE_ACME_DOMAINS` is the single on-switch.
#[derive(Debug)]
pub(crate) struct AcmeSettings {
    domains: Vec<String>,
    contact: String,
    cache: PathBuf,
    directory: String,
    bind: SocketAddr,
    extra_root: Option<PathBuf>,
}

impl AcmeSettings {
    /// Resolve the flat clap fields: `None` when ACME is off (an empty domain list — the compiled-in
    /// feature alone changes nothing); a plain error naming the missing flag when it is on but incomplete.
    pub(crate) fn from_config(cfg: &Config) -> Result<Option<Self>> {
        let domains: Vec<String> = cfg
            .acme_domain
            .iter()
            .map(|d| d.trim())
            .filter(|d| !d.is_empty())
            .map(str::to_owned)
            .collect();
        if domains.is_empty() {
            return Ok(None);
        }
        let contact = cfg.acme_contact.clone().context(
            "the ACME TLS listener is on (--acme-domain / TOPOS_PLANE_ACME_DOMAINS is non-empty) but \
             --acme-contact / TOPOS_PLANE_ACME_CONTACT is not set (e.g. mailto:ops@example.com)",
        )?;
        let cache = cfg.acme_cache.clone().context(
            "the ACME TLS listener is on (--acme-domain / TOPOS_PLANE_ACME_DOMAINS is non-empty) but \
             --acme-cache / TOPOS_PLANE_ACME_CACHE is not set (a persistent directory, e.g. /data/acme)",
        )?;
        Ok(Some(Self {
            domains,
            contact,
            cache,
            directory: cfg.acme_directory.clone(),
            bind: cfg.acme_bind,
            extra_root: cfg.acme_extra_root.clone(),
        }))
    }
}

/// Serve BOTH listeners: the plain-HTTP listener exactly as the default path does (same bind, same
/// router, same connect-info wiring), PLUS the ACME TLS listener. Returns when either serve loop fails.
pub(crate) async fn serve_with_tls(
    plain_bind: SocketAddr,
    settings: AcmeSettings,
    state: PlaneState,
) -> Result<()> {
    // ONE process-default rustls crypto provider (ring — the backend everything TLS in this binary
    // already rides). `Err` means a default was installed already, which is exactly the state we want;
    // installing it here makes a two-provider builder panic structurally impossible.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let client_config = acme_client_config(settings.extra_root.as_deref())?;
    let mut acme_state = AcmeConfig::new_with_client_config(&settings.domains, client_config)
        .contact([settings.contact.as_str()])
        .cache(DirCache::new(settings.cache.clone()))
        .directory(&settings.directory)
        .state();
    let acceptor = acme_state.axum_acceptor(acme_state.default_rustls_config());

    // Drain the ACME lifecycle into the log — deployment/issuance progress and errors only, NEVER key
    // material (`EventOk` is a plain fieldless enum; `EventError` renders as its error chain).
    tokio::spawn(async move {
        while let Some(event) = acme_state.next().await {
            match event {
                Ok(ok) => tracing::info!(event = ?ok, "acme"),
                Err(err) => tracing::warn!(error = %err, "acme"),
            }
        }
    });

    // The TLS listener: a std bind handed to axum-server (nonblocking, as tokio's `from_std` requires).
    let tls_listener = std::net::TcpListener::bind(settings.bind)
        .with_context(|| format!("binding {} (acme tls)", settings.bind))?;
    tls_listener
        .set_nonblocking(true)
        .context("setting the acme tls listener non-blocking")?;
    let tls_server = axum_server::from_tcp(tls_listener)
        .with_context(|| format!("registering {} (acme tls)", settings.bind))?;

    let plain_listener = tokio::net::TcpListener::bind(plain_bind)
        .await
        .with_context(|| format!("binding {plain_bind}"))?;
    tracing::info!(
        addr = %plain_bind,
        tls_addr = %settings.bind,
        domains = ?settings.domains,
        directory = %settings.directory,
        "topos-plane listening (plain http + acme tls)"
    );

    // Both listeners serve the SAME router; `ConnectInfo<SocketAddr>` is wired on both so the rate
    // limiter can key on the peer IP when no credential rides.
    let app = router(state);
    let plain = axum::serve(
        plain_listener,
        app.clone()
            .into_make_service_with_connect_info::<SocketAddr>(),
    );
    let tls = tls_server
        .acceptor(acceptor)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>());
    tokio::try_join!(
        async { plain.await.context("serving the plane (plain http)") },
        async { tls.await.context("serving the plane (acme tls)") },
    )?;
    Ok(())
}

/// The ACME CLIENT's TLS trust store: the WebPKI roots, plus the optional `--acme-extra-root` PEM (a
/// private/test ACME directory's root — never needed against a public CA). This trust decides only which
/// DIRECTORY the plane will talk to; the certificates it serves are whatever that directory issues.
fn acme_client_config(extra_root: Option<&Path>) -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = extra_root {
        use rustls_pki_types::pem::PemObject;
        let mut added = 0usize;
        for cert in rustls_pki_types::CertificateDer::pem_file_iter(path)
            .with_context(|| format!("reading the ACME extra root PEM at {}", path.display()))?
        {
            let cert =
                cert.with_context(|| format!("parsing a certificate in {}", path.display()))?;
            roots
                .add(cert)
                .with_context(|| format!("adding a certificate from {}", path.display()))?;
            added += 1;
        }
        anyhow::ensure!(
            added > 0,
            "no certificates found in the ACME extra root PEM at {}",
            path.display()
        );
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("selecting TLS protocol versions for the ACME client")?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}
