//! What the Google providers share: where each of Google's services can be reached.
//!
//! Drive and the OAuth token endpoint are two hosts, and a deployment that is not
//! production Google may put either anywhere. That belongs to no one accessor, so it
//! lives beside them rather than inside whichever one happened to need it first.
//!
//! There were five here once, when a document was read through the Docs, Sheets and
//! Slides APIs. An export goes through Drive's own `exportLinks` instead, and the host
//! that serves it is Google's to name in the redirect rather than ours to configure.

use serde::{Deserialize, Serialize};

/// The OAuth origin, shared by every provider here: one token endpoint serves them all.
pub(crate) const OAUTH_ORIGIN: &str = "https://oauth2.googleapis.com";

/// Where to reach each Google service. `None` = the real host.
///
/// Google gives every service its own host, and no single origin stands in for all of
/// them, so each is overridable on its own. Whatever is set here is an *origin*: this
/// code appends only the suffix the official API uses, so the same paths address a
/// mock and production alike.
///
/// Deployment-level only: the token endpoint receives the app's client secret, so
/// none of this may be user-suppliable.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Origins {
    /// Serves the OAuth token endpoint (`{oauth}/token`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<String>,
    /// Serves `drive/v3` (`{drive}/v3/files`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive: Option<String>,
}

impl Origins {
    /// Whether nothing is overridden, so the field can stay out of a serialized
    /// config.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Both behind one host, laid out the way Google's own paths read: `{host}/oauth2`
    /// and `{host}/drive`. A convenience for a deployment that fronts them, not a
    /// substitute for the per-service knobs.
    pub fn behind(host: &str) -> Self {
        let h = host.trim_end_matches('/');
        let at = |service: &str| Some(format!("{h}/{service}"));
        Self {
            oauth: at("oauth2"),
            drive: at("drive"),
        }
    }

    /// `over` if set, else `default`, without a trailing slash.
    pub(crate) fn origin(over: &Option<String>, default: &str) -> String {
        over.as_deref()
            .unwrap_or(default)
            .trim_end_matches('/')
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An override replaces an origin and nothing else, so whoever sets one does not
    /// also have to know which path this code would have appended.
    /// An override replaces an origin and nothing else, so whoever sets one does not
    /// also have to know which path this code would have appended.
    #[test]
    fn an_override_replaces_only_its_own_origin() {
        let o = Origins {
            drive: Some("http://localhost:9000/drive-api/".into()),
            ..Default::default()
        };
        assert_eq!(
            Origins::origin(&o.drive, "https://www.googleapis.com/drive"),
            "http://localhost:9000/drive-api",
            "trailing slash trimmed, so the caller need not care"
        );
        assert_eq!(
            Origins::origin(&o.oauth, "https://oauth2.googleapis.com"),
            "https://oauth2.googleapis.com",
            "the other stays on Google"
        );
    }

    /// One host fronting all of them is the common deployment, and it reads the way
    /// Google's own paths do.
    #[test]
    fn behind_one_host_lays_the_services_out_by_name() {
        let o = Origins::behind("https://mock.example.com/");
        assert_eq!(o.oauth.as_deref(), Some("https://mock.example.com/oauth2"));
        assert_eq!(o.drive.as_deref(), Some("https://mock.example.com/drive"));
        assert!(!o.is_default());
        assert!(Origins::default().is_default(), "nothing set stays absent");
    }
}
