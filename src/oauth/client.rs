//! Centralized OAuth client-credential resolution.
//!
//! A single fallback chain — **pre-registered → CIMD → DCR → manual** — shared
//! by the JIT 401 interceptor (`adapter/oauth/jit.rs`) and the Add Server setup
//! flow (`management.rs`). When the authorization server advertises a Client ID
//! Metadata Document (CIMD) and there is no pre-registered client, the relay
//! authenticates as a zero-config public client using
//! [`ENDARA_CLIENT_METADATA_URL`] as the `client_id` (no secret, no DCR).
//!
//! An *explicit* user-supplied client_id (e.g. pasted into Add Server) wins for
//! that flow; the chain's "manual = last" ordering applies to the automatic/JIT
//! path, which never supplies a manual client_id.

use std::future::Future;

/// Endara's hosted Client ID Metadata Document (CIMD). Used as the `client_id`
/// for zero-config public-client authentication when the authorization server
/// advertises `client_id_metadata_document_supported`.
pub const ENDARA_CLIENT_METADATA_URL: &str = "https://endara.ai/oauth/client-metadata.json";

/// Which path produced the resolved client credentials. Surfaced in structured
/// logs as `client_registration=<as_str>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientRegistration {
    Preregistered,
    Cimd,
    Dcr,
    Manual,
}

impl ClientRegistration {
    pub fn as_str(self) -> &'static str {
        match self {
            ClientRegistration::Preregistered => "preregistered",
            ClientRegistration::Cimd => "cimd",
            ClientRegistration::Dcr => "dcr",
            ClientRegistration::Manual => "manual",
        }
    }
}

/// Result of a successful Dynamic Client Registration, returned by the DCR
/// closure passed to [`resolve_client`].
#[derive(Debug)]
pub struct DcrOutcome {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub client_secret_expires_at: u64,
}

/// Inputs gathered by the call site before resolution.
pub struct ClientInputs {
    /// Explicit user-supplied client_id (+ optional secret), e.g. pasted into
    /// Add Server. When present it wins over every automatic path for this flow.
    pub explicit_manual: Option<(String, Option<String>)>,
    /// Pre-registered / stored credentials already validated as reusable
    /// (issuer-bound) by the caller.
    pub preregistered: Option<(String, Option<String>)>,
    /// Whether the AS advertises Client ID Metadata Document (CIMD) support.
    pub cimd_supported: bool,
    /// The AS registration endpoint, when DCR is available.
    pub registration_endpoint: Option<String>,
}

/// Resolved client credentials plus the path taken.
#[derive(Debug)]
pub struct ResolvedClient {
    pub client_id: String,
    pub client_secret: Option<String>,
    /// Seconds-since-epoch expiry of `client_secret` from DCR (0 = never / N/A).
    pub client_secret_expires_at: u64,
    pub registration: ClientRegistration,
}

/// Error from [`resolve_client`].
#[derive(Debug, thiserror::Error)]
pub enum ClientResolveError<E> {
    #[error("dynamic client registration failed: {0}")]
    Dcr(E),
    #[error("no client credentials: server does not support DCR/CIMD and none are stored")]
    NoCredentials,
}

/// Resolve client credentials following **pre-registered → CIMD → DCR →
/// manual**, with one exception: an *explicit* user-supplied client_id
/// (`explicit_manual`) wins for that flow.
///
/// `dcr` is awaited ONLY when the DCR path is selected — i.e. no explicit,
/// pre-registered, or CIMD credentials are available but a registration
/// endpoint exists. CIMD and pre-registered paths therefore never trigger a
/// network registration.
pub async fn resolve_client<F, Fut, E>(
    inputs: ClientInputs,
    dcr: F,
) -> Result<ResolvedClient, ClientResolveError<E>>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<DcrOutcome, E>>,
{
    if let Some((client_id, client_secret)) = inputs.explicit_manual {
        return Ok(ResolvedClient {
            client_id,
            client_secret,
            client_secret_expires_at: 0,
            registration: ClientRegistration::Manual,
        });
    }
    if let Some((client_id, client_secret)) = inputs.preregistered {
        return Ok(ResolvedClient {
            client_id,
            client_secret,
            client_secret_expires_at: 0,
            registration: ClientRegistration::Preregistered,
        });
    }
    if inputs.cimd_supported {
        return Ok(ResolvedClient {
            client_id: ENDARA_CLIENT_METADATA_URL.to_string(),
            client_secret: None,
            client_secret_expires_at: 0,
            registration: ClientRegistration::Cimd,
        });
    }
    if let Some(reg_endpoint) = inputs.registration_endpoint {
        let outcome = dcr(reg_endpoint).await.map_err(ClientResolveError::Dcr)?;
        return Ok(ResolvedClient {
            client_id: outcome.client_id,
            client_secret: outcome.client_secret,
            client_secret_expires_at: outcome.client_secret_expires_at,
            registration: ClientRegistration::Dcr,
        });
    }
    Err(ClientResolveError::NoCredentials)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn ok_dcr() -> DcrOutcome {
        DcrOutcome {
            client_id: "dcr-client".to_string(),
            client_secret: Some("dcr-secret".to_string()),
            client_secret_expires_at: 0,
        }
    }

    #[tokio::test]
    async fn explicit_manual_wins_over_everything() {
        let called = AtomicBool::new(false);
        let resolved = resolve_client(
            ClientInputs {
                explicit_manual: Some(("manual-id".into(), Some("manual-secret".into()))),
                preregistered: Some(("pre".into(), None)),
                cimd_supported: true,
                registration_endpoint: Some("https://as/register".into()),
            },
            |_ep| {
                called.store(true, Ordering::SeqCst);
                async { Ok::<_, ()>(ok_dcr()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(resolved.registration, ClientRegistration::Manual);
        assert_eq!(resolved.client_id, "manual-id");
        assert_eq!(resolved.client_secret.as_deref(), Some("manual-secret"));
        assert!(!called.load(Ordering::SeqCst), "DCR must not be invoked");
    }

    #[tokio::test]
    async fn preregistered_wins_over_cimd_and_dcr() {
        let called = AtomicBool::new(false);
        let resolved = resolve_client(
            ClientInputs {
                explicit_manual: None,
                preregistered: Some(("pre-id".into(), Some("pre-secret".into()))),
                cimd_supported: true,
                registration_endpoint: Some("https://as/register".into()),
            },
            |_ep| {
                called.store(true, Ordering::SeqCst);
                async { Ok::<_, ()>(ok_dcr()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(resolved.registration, ClientRegistration::Preregistered);
        assert_eq!(resolved.client_id, "pre-id");
        assert!(!called.load(Ordering::SeqCst), "DCR must not be invoked");
    }

    #[tokio::test]
    async fn cimd_chosen_without_pre_registered_and_skips_dcr() {
        let called = AtomicBool::new(false);
        let resolved = resolve_client(
            ClientInputs {
                explicit_manual: None,
                preregistered: None,
                cimd_supported: true,
                registration_endpoint: Some("https://as/register".into()),
            },
            |_ep| {
                called.store(true, Ordering::SeqCst);
                async { Ok::<_, ()>(ok_dcr()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(resolved.registration, ClientRegistration::Cimd);
        assert_eq!(resolved.client_id, ENDARA_CLIENT_METADATA_URL);
        assert!(resolved.client_secret.is_none());
        assert!(
            !called.load(Ordering::SeqCst),
            "DCR must not be invoked when CIMD is advertised"
        );
    }

    #[tokio::test]
    async fn dcr_chosen_when_cimd_absent() {
        let called = AtomicBool::new(false);
        let resolved = resolve_client(
            ClientInputs {
                explicit_manual: None,
                preregistered: None,
                cimd_supported: false,
                registration_endpoint: Some("https://as/register".into()),
            },
            |ep| {
                assert_eq!(ep, "https://as/register");
                called.store(true, Ordering::SeqCst);
                async { Ok::<_, ()>(ok_dcr()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(resolved.registration, ClientRegistration::Dcr);
        assert_eq!(resolved.client_id, "dcr-client");
        assert_eq!(resolved.client_secret.as_deref(), Some("dcr-secret"));
        assert!(called.load(Ordering::SeqCst), "DCR must be invoked");
    }

    #[tokio::test]
    async fn no_credentials_when_nothing_available() {
        let result = resolve_client(
            ClientInputs {
                explicit_manual: None,
                preregistered: None,
                cimd_supported: false,
                registration_endpoint: None,
            },
            |_ep| async { Ok::<_, ()>(ok_dcr()) },
        )
        .await;
        assert!(matches!(result, Err(ClientResolveError::NoCredentials)));
    }

    #[tokio::test]
    async fn dcr_error_propagates() {
        let result = resolve_client(
            ClientInputs {
                explicit_manual: None,
                preregistered: None,
                cimd_supported: false,
                registration_endpoint: Some("https://as/register".into()),
            },
            |_ep| async { Err::<DcrOutcome, _>("boom") },
        )
        .await;
        match result {
            Err(ClientResolveError::Dcr(e)) => assert_eq!(e, "boom"),
            other => panic!("expected Dcr error, got {other:?}"),
        }
    }
}
