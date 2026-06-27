//! Static identity-provider templates (END-19, Wave 3).
//!
//! One source of truth for the provider table the desktop "Add organization" UI
//! renders and the `POST /api/organizations` handler resolves issuer URLs from.
//! Each template maps a provider id to an issuer URL pattern carrying a single
//! `{slug}` placeholder (or no placeholder for fixed-issuer providers like
//! Google), plus a human-readable hint describing what the slug is.
//!
//! `custom` carries no pattern: callers paste a full issuer URL via the request
//! `idp` field instead, and it is validated by discovery like any other issuer.

/// A single identity-provider template surfaced by `GET /api/idp-providers`.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct IdpProvider {
    /// Stable provider id: `okta`, `entra`, `google`, `ping`, or `custom`.
    pub id: &'static str,
    /// Human-readable provider name for display.
    pub name: &'static str,
    /// Issuer URL pattern. Carries a `{slug}` placeholder for tenant-scoped
    /// providers, a fixed issuer with no placeholder for Google, or `None` for
    /// `custom` (the caller pastes a full issuer URL instead).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer_pattern: Option<&'static str>,
    /// Human-readable hint describing the slug the user must supply. `None` for
    /// providers that take no slug (Google) or a free-form issuer (custom).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug_hint: Option<&'static str>,
}

impl IdpProvider {
    /// Resolve the issuer URL from this template and an optional `slug`.
    ///
    /// - Templated providers (`{slug}` placeholder) require a non-empty slug and
    ///   substitute it after rejecting characters that could break out of the
    ///   issuer host/path.
    /// - Fixed-issuer providers (Google) ignore the slug and return the pattern.
    /// - `custom` (no pattern) returns an error: the caller must supply a full
    ///   issuer URL via the request `idp` field instead.
    pub fn build_issuer(&self, slug: Option<&str>) -> Result<String, String> {
        let Some(pattern) = self.issuer_pattern else {
            return Err(format!(
                "provider '{}' has no issuer template; supply a custom issuer URL",
                self.id
            ));
        };
        if !pattern.contains("{slug}") {
            // Fixed issuer (e.g. Google): the slug, if any, is irrelevant.
            return Ok(pattern.to_string());
        }
        let slug = slug
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("provider '{}' requires a 'slug'", self.id))?;
        // Reject slugs that could alter the issuer's host or path structure.
        if slug.contains(['/', ':', '?', '#', '@', '\\', ' ']) || slug.contains("..") {
            return Err(format!("invalid slug '{slug}' for provider '{}'", self.id));
        }
        Ok(pattern.replace("{slug}", slug))
    }
}

/// The static provider table — the single source of truth shared by the
/// `GET /api/idp-providers` response and `POST /api/organizations` resolution.
pub const IDP_PROVIDERS: &[IdpProvider] = &[
    IdpProvider {
        id: "okta",
        name: "Okta",
        issuer_pattern: Some("https://{slug}.okta.com"),
        slug_hint: Some("Your Okta org subdomain (e.g. \"acme\" for https://acme.okta.com)"),
    },
    IdpProvider {
        id: "entra",
        name: "Microsoft Entra ID",
        issuer_pattern: Some("https://login.microsoftonline.com/{slug}/v2.0"),
        slug_hint: Some("Your Entra tenant ID or domain (e.g. \"contoso.onmicrosoft.com\")"),
    },
    IdpProvider {
        id: "google",
        name: "Google",
        issuer_pattern: Some("https://accounts.google.com"),
        slug_hint: None,
    },
    IdpProvider {
        id: "ping",
        name: "PingOne",
        issuer_pattern: Some("https://auth.pingone.com/{slug}/as"),
        slug_hint: Some("Your PingOne environment ID"),
    },
    IdpProvider {
        id: "custom",
        name: "Custom",
        issuer_pattern: None,
        slug_hint: None,
    },
];

/// Look up a provider template by its stable id.
pub fn find_provider(id: &str) -> Option<&'static IdpProvider> {
    IDP_PROVIDERS.iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn okta_builds_issuer_from_slug() {
        let p = find_provider("okta").unwrap();
        assert_eq!(
            p.build_issuer(Some("acme")).unwrap(),
            "https://acme.okta.com"
        );
    }

    #[test]
    fn entra_and_ping_substitute_slug() {
        assert_eq!(
            find_provider("entra")
                .unwrap()
                .build_issuer(Some("tenant-1"))
                .unwrap(),
            "https://login.microsoftonline.com/tenant-1/v2.0"
        );
        assert_eq!(
            find_provider("ping")
                .unwrap()
                .build_issuer(Some("env-9"))
                .unwrap(),
            "https://auth.pingone.com/env-9/as"
        );
    }

    #[test]
    fn google_ignores_slug_and_needs_none() {
        let p = find_provider("google").unwrap();
        assert_eq!(p.build_issuer(None).unwrap(), "https://accounts.google.com");
        assert_eq!(
            p.build_issuer(Some("x")).unwrap(),
            "https://accounts.google.com"
        );
    }

    #[test]
    fn templated_provider_requires_non_empty_slug() {
        let p = find_provider("okta").unwrap();
        assert!(p.build_issuer(None).is_err());
        assert!(p.build_issuer(Some("   ")).is_err());
    }

    #[test]
    fn rejects_slug_with_path_traversal_or_separators() {
        let p = find_provider("okta").unwrap();
        assert!(p.build_issuer(Some("a/b")).is_err());
        assert!(p.build_issuer(Some("a:b")).is_err());
        assert!(p.build_issuer(Some("..")).is_err());
    }

    #[test]
    fn custom_has_no_template() {
        let p = find_provider("custom").unwrap();
        assert!(p.issuer_pattern.is_none());
        assert!(p.build_issuer(Some("anything")).is_err());
    }
}
