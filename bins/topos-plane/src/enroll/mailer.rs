//! The passcode mailer seam — a private TEST seam, NOT a product port.
//!
//! Mirrors the `Limiter` / `LargeObjectStore` precedent: a `pub(crate)` trait with a real SMTP impl
//! (`SmtpMailer`), a silent self-host default (`NoopMailer`), and a recording test double (`FakeMailer`).
//! Construction is INTERNAL to [`PlaneState`](crate::PlaneState) (`with_enroll_config` builds the mailer from
//! the config); nothing outside the crate injects one except the test-gated `with_mailer` shim.
//!
//! **Redaction.** The 6-digit code is wrapped in [`Passcode`], whose hand-written `Debug` prints
//! `<redacted>` — the code never reaches a log or panic message. `SmtpMailer`'s hand-written `Debug` omits
//! the transport (it holds the relay credentials), and [`SmtpConfig`]'s omits the user + password. The mailer
//! trait stays SYNC + dyn-compatible (no async-trait): the handler runs the blocking SMTP send on
//! `tokio::task::spawn_blocking`, and the passcode request is fire-and-forget (spawn the send, return a
//! constant-shaped ack) so neither the response body nor its latency leaks whether the address was rostered.

use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

/// Mail a verification passcode. The send is SYNC + dyn-compatible on purpose — the handler offloads the
/// blocking SMTP round-trip to `tokio::task::spawn_blocking`, so it never stalls the async runtime.
pub(crate) trait Mailer: Send + Sync + std::fmt::Debug {
    /// Send the verification `code` to `to`, with the surrounding `ctx` (workspace name, base URL) for the
    /// message body.
    ///
    /// # Errors
    /// [`MailError`] if the message cannot be built or the relay rejects/refuses the send.
    fn send_passcode(&self, to: &str, code: &Passcode, ctx: &MailContext) -> Result<(), MailError>;
}

/// A 6-digit passcode to mail. The plaintext is wrapped so its `Debug` REDACTS — the code reaches the SMTP
/// body once and NEVER a log or panic message (the `FollowEntry` / `PlaneSigner` redaction precedent).
pub(crate) struct Passcode(String);

impl Passcode {
    /// Wrap a plaintext passcode (the handler passes the one [`plane_store`] returned).
    pub(crate) fn new(code: String) -> Self {
        Self(code)
    }

    /// The raw code — for the mail body ONLY (never logged).
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Redacting `Debug` — prints exactly `<redacted>`, never the code (the crate lints
/// `missing_debug_implementations`, so a field needs `Debug`; a derived one would print the code).
impl std::fmt::Debug for Passcode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The surrounding context a passcode email renders with (no secret — the code rides in [`Passcode`]).
#[derive(Debug, Clone)]
pub(crate) struct MailContext {
    /// The workspace display name (for the subject + body).
    pub(crate) workspace_display_name: String,
    /// The plane's public base URL (for the verification link in the body).
    pub(crate) base_url: String,
}

/// A mail-send failure. Deliberately COARSE — a static stage label, never the relay address, credentials, or
/// recipient (SMTP creds never reach a log; the send is fire-and-forget, so a failure is a bare stage,
/// never an enumeration oracle).
#[derive(Debug, thiserror::Error)]
pub(crate) enum MailError {
    /// The message could not be assembled (a bad from/to address or a body-build failure).
    #[error("failed to build the passcode message")]
    Build,
    /// The relay refused or could not be reached.
    #[error("failed to send the passcode message")]
    Send,
}

/// The SMTP relay configuration (host, port, credentials, from-address). `pub` because it is a field of the
/// public [`EnrollConfig`](crate::EnrollConfig); its `Debug` REDACTS the user + password.
#[derive(Clone)]
pub struct SmtpConfig {
    /// The SMTP relay hostname.
    pub host: String,
    /// The SMTP relay port.
    pub port: u16,
    /// The SMTP username (a credential — never logged).
    pub user: String,
    /// The SMTP password (a credential — never logged).
    pub pass: String,
    /// The envelope/from address mail is sent from.
    pub from: String,
}

/// Redacting `Debug` — shows host/port/from, OMITS the user + password (a credential is never logged).
impl std::fmt::Debug for SmtpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("from", &self.from)
            .finish_non_exhaustive()
    }
}

/// A real SMTP mailer over `lettre`'s blocking `SmtpTransport` (rustls — no C/openssl). Holds the configured
/// transport (the relay + credentials) and the from-address.
pub(crate) struct SmtpMailer {
    transport: SmtpTransport,
    from: Mailbox,
}

impl SmtpMailer {
    /// Build a mailer from the relay config: parse the from-address, attach the credentials, and build a
    /// blocking rustls `SmtpTransport`.
    ///
    /// # Errors
    /// [`anyhow::Error`] if the from-address is malformed or the relay cannot be constructed. The error
    /// NEVER contains the username or password (they are infallibly attached after, never parsed).
    pub(crate) fn from_smtp_config(cfg: &SmtpConfig) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let from: Mailbox = cfg.from.parse().context("parsing the SMTP from-address")?;
        let credentials = Credentials::new(cfg.user.clone(), cfg.pass.clone());
        let transport = SmtpTransport::relay(&cfg.host)
            .context("building the SMTP relay")?
            .port(cfg.port)
            .credentials(credentials)
            .build();
        Ok(Self { transport, from })
    }
}

/// Redacting `Debug` — OMITS the transport (it holds the relay credentials); shows only the from-address.
impl std::fmt::Debug for SmtpMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpMailer")
            .field("from", &self.from)
            .finish_non_exhaustive()
    }
}

impl Mailer for SmtpMailer {
    fn send_passcode(&self, to: &str, code: &Passcode, ctx: &MailContext) -> Result<(), MailError> {
        let to: Mailbox = to.parse().map_err(|_| MailError::Build)?;
        let body = format!(
            "Your Topos verification code for {} is {}.\n\nEnter it at {}/device to finish connecting your agent.\n\
             If you didn't request this, you can ignore this email.\n",
            ctx.workspace_display_name,
            code.as_str(),
            ctx.base_url,
        );
        let message = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject("Your Topos verification code")
            .header(ContentType::TEXT_PLAIN)
            .body(body)
            .map_err(|_| MailError::Build)?;
        // Map the lettre error to a coarse stage — the relay address/response never enters MailError.
        self.transport.send(&message).map_err(|_| MailError::Send)?;
        Ok(())
    }
}

/// The no-SMTP self-host default — silently drops the passcode (the bootstrap simply won't advertise the
/// passcode method when no SMTP is configured).
#[derive(Debug, Default)]
pub(crate) struct NoopMailer;

impl Mailer for NoopMailer {
    fn send_passcode(
        &self,
        _to: &str,
        _code: &Passcode,
        _ctx: &MailContext,
    ) -> Result<(), MailError> {
        Ok(())
    }
}

/// A recording test double — captures each send (recipient + plaintext code) so a route test can assert the
/// passcode WITHOUT an SMTP server. Test-gated (`test` / `test-fixtures`), so it never ships.
#[cfg(any(test, feature = "test-fixtures"))]
#[derive(Clone, Debug, Default)]
pub(crate) struct FakeMailer {
    sent: std::sync::Arc<std::sync::Mutex<Vec<SentMail>>>,
}

/// One recorded send. `code` is the PLAINTEXT on purpose — the whole point of the fake is to let a test read
/// the code without SMTP. Test-gated, so it never ships.
#[cfg(any(test, feature = "test-fixtures"))]
#[derive(Debug, Clone)]
pub(crate) struct SentMail {
    /// The recipient address.
    pub(crate) to: String,
    /// The plaintext passcode (test readback only).
    pub(crate) code: String,
}

#[cfg(any(test, feature = "test-fixtures"))]
impl FakeMailer {
    /// A snapshot of everything mailed so far (a test asserts + reads the code off this).
    pub(crate) fn sent(&self) -> Vec<SentMail> {
        self.sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[cfg(any(test, feature = "test-fixtures"))]
impl Mailer for FakeMailer {
    fn send_passcode(
        &self,
        to: &str,
        code: &Passcode,
        _ctx: &MailContext,
    ) -> Result<(), MailError> {
        self.sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(SentMail {
                to: to.to_owned(),
                code: code.as_str().to_owned(),
            });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_mailer_records_the_send_and_passcode_debug_is_redacted() {
        // The Passcode's Debug NEVER shows the code (the FollowEntry / PlaneSigner redaction precedent).
        let code = Passcode::new("123456".to_owned());
        let shown = format!("{code:?}");
        assert_eq!(shown, "<redacted>");
        assert!(
            !shown.contains("123456"),
            "the 6-digit code must never appear in Debug"
        );

        // The FakeMailer records the send so a route test can assert + READ the code without SMTP.
        let mailer = FakeMailer::default();
        let ctx = MailContext {
            workspace_display_name: "Acme".to_owned(),
            base_url: "https://plane.test".to_owned(),
        };
        mailer.send_passcode("alice@acme.com", &code, &ctx).unwrap();
        let sent = mailer.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, "alice@acme.com");
        assert_eq!(sent[0].code, "123456");
    }

    #[test]
    fn smtp_config_debug_redacts_user_and_password() {
        let cfg = SmtpConfig {
            host: "smtp.acme.com".to_owned(),
            port: 465,
            user: "postmaster@acme.com".to_owned(),
            pass: "s3cr3t-app-password".to_owned(),
            from: "Topos <no-reply@acme.com>".to_owned(),
        };
        let shown = format!("{cfg:?}");
        assert!(
            !shown.contains("s3cr3t-app-password"),
            "password must be redacted"
        );
        assert!(
            !shown.contains("postmaster@acme.com"),
            "username must be redacted"
        );
        assert!(shown.contains("smtp.acme.com"), "host is fine to show");
    }
}
