//! What the Google providers share: where each of Google's services can be reached.
//!
//! Drive, Docs, Sheets and Slides are four APIs on four hosts, and a deployment that is
//! not production Google may put any of them anywhere. That belongs to no one accessor,
//! so it lives beside them rather than inside whichever one happened to need it first.

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
    /// Serves the Docs API (`{docs}/v1/documents/…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
    /// Serves the Sheets API (`{sheets}/v4/spreadsheets/…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheets: Option<String>,
    /// Serves the Slides API (`{slides}/v1/presentations/…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slides: Option<String>,
}

impl Origins {
    /// Whether nothing is overridden, so the field can stay out of a serialized
    /// config.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Every service behind one host, laid out the way Google's own paths read:
    /// `{host}/gmail`, `{host}/oauth2`, `{host}/drive` and so on. A convenience for a
    /// deployment that fronts all of them, not a substitute for the per-service knobs.
    pub fn behind(host: &str) -> Self {
        let h = host.trim_end_matches('/');
        let at = |service: &str| Some(format!("{h}/{service}"));
        Self {
            oauth: at("oauth2"),
            drive: at("drive"),
            docs: at("docs"),
            sheets: at("sheets"),
            slides: at("slides"),
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
    #[test]
    fn an_override_replaces_only_its_own_origin() {
        let o = Origins {
            sheets: Some("http://localhost:9000/sheets-api/".into()),
            ..Default::default()
        };
        assert_eq!(
            Origins::origin(&o.sheets, "https://sheets.googleapis.com"),
            "http://localhost:9000/sheets-api",
            "trailing slash trimmed, so the caller need not care"
        );
        assert_eq!(
            Origins::origin(&o.docs, "https://docs.googleapis.com"),
            "https://docs.googleapis.com",
            "the rest stay on Google"
        );
    }

    /// One host fronting all of them is the common deployment, and it reads the way
    /// Google's own paths do.
    #[test]
    fn behind_one_host_lays_the_services_out_by_name() {
        let o = Origins::behind("https://mock.example.com/");
        assert_eq!(o.oauth.as_deref(), Some("https://mock.example.com/oauth2"));
        assert_eq!(o.drive.as_deref(), Some("https://mock.example.com/drive"));
        assert_eq!(o.docs.as_deref(), Some("https://mock.example.com/docs"));
        assert_eq!(o.sheets.as_deref(), Some("https://mock.example.com/sheets"));
        assert_eq!(o.slides.as_deref(), Some("https://mock.example.com/slides"));
        assert!(!o.is_default());
        assert!(Origins::default().is_default(), "nothing set stays absent");
    }
}
