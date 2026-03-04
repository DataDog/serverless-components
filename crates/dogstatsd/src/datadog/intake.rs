// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use derive_more::{Display, Into};
use regex::Regex;
use std::sync::OnceLock;

// TODO: Move to the more ergonomic LazyLock when MSRV is 1.80
static SITE_RE: OnceLock<Regex> = OnceLock::new();
fn get_site_re() -> &'static Regex {
    #[allow(clippy::expect_used)]
    SITE_RE.get_or_init(|| Regex::new(r"^[a-zA-Z0-9._:-]+$").expect("invalid regex"))
}
static URL_PREFIX_RE: OnceLock<Regex> = OnceLock::new();
fn get_url_prefix_re() -> &'static Regex {
    #[allow(clippy::expect_used)]
    URL_PREFIX_RE.get_or_init(|| Regex::new(r"^https?://[a-zA-Z0-9._:-]+$").expect("invalid regex"))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Display, Into)]
pub struct Site(String);

#[derive(thiserror::Error, Debug, Clone, PartialEq)]
#[error("Invalid site: {0}")]
pub struct SiteError(String);

impl Site {
    pub fn new(site: String) -> Result<Self, SiteError> {
        // Datadog sites are generally domain names. In particular, they shouldn't have any slashes
        // in them. We expect this to be coming from a `DD_SITE` environment variable or the `site`
        // config field.
        if get_site_re().is_match(&site) {
            Ok(Site(site))
        } else {
            Err(SiteError(site))
        }
    }
}

#[derive(thiserror::Error, Debug, Clone, PartialEq)]
#[error("Invalid URL prefix: {0}")]
pub struct UrlPrefixError(String);

fn validate_url_prefix(prefix: &str) -> Result<(), UrlPrefixError> {
    if get_url_prefix_re().is_match(prefix) {
        Ok(())
    } else {
        Err(UrlPrefixError(prefix.to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Display, Into)]
pub struct DdUrl(String);

impl DdUrl {
    pub fn new(prefix: String) -> Result<Self, UrlPrefixError> {
        validate_url_prefix(&prefix)?;
        Ok(Self(prefix))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Display, Into)]
pub struct DdDdUrl(String);

impl DdDdUrl {
    pub fn new(prefix: String) -> Result<Self, UrlPrefixError> {
        validate_url_prefix(&prefix)?;
        Ok(Self(prefix))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Display, Into)]
pub struct MetricsIntakeUrlPrefixOverride(String);

impl MetricsIntakeUrlPrefixOverride {
    pub fn maybe_new(dd_url: Option<DdUrl>, dd_dd_url: Option<DdDdUrl>) -> Option<Self> {
        match (dd_url, dd_dd_url) {
            (None, None) => None,
            (_, Some(dd_dd_url)) => Some(Self(dd_dd_url.into())),
            (Some(dd_url), None) => Some(Self(dd_url.into())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Display)]
pub struct MetricsIntakeUrlPrefix(String);

#[derive(thiserror::Error, Debug, Clone, PartialEq)]
#[error("Missing intake URL configuration")]
pub struct MissingIntakeUrlError;

impl MetricsIntakeUrlPrefix {
    #[inline]
    pub fn new(
        site: Option<Site>,
        overridden_prefix: Option<MetricsIntakeUrlPrefixOverride>,
    ) -> Result<Self, MissingIntakeUrlError> {
        match (site, overridden_prefix) {
            (None, None) => Err(MissingIntakeUrlError),
            (_, Some(prefix)) => Ok(Self::new_expect_validated(prefix.into())),
            (Some(site), None) => Ok(Self::from_site(site)),
        }
    }

    #[inline]
    fn new_expect_validated(validated_prefix: String) -> Self {
        #[allow(clippy::expect_used)]
        validate_url_prefix(&validated_prefix).expect("Invalid URL prefix");

        Self(validated_prefix)
    }

    #[inline]
    fn from_site(site: Site) -> Self {
        Self(format!("https://api.{site}"))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn override_can_be_empty() {
        assert_eq!(MetricsIntakeUrlPrefixOverride::maybe_new(None, None), None);
    }

    #[test]
    fn override_prefers_dd_dd_url() {
        assert_eq!(
            MetricsIntakeUrlPrefixOverride::maybe_new(
                Some(DdUrl::new("http://a_dd_url".to_string()).unwrap()),
                Some(DdDdUrl::new("https://a_dd_dd_url".to_string()).unwrap())
            ),
            Some(MetricsIntakeUrlPrefixOverride(
                "https://a_dd_dd_url".to_string()
            ))
        );
    }

    #[test]
    fn override_will_take_dd_url() {
        assert_eq!(
            MetricsIntakeUrlPrefixOverride::maybe_new(
                Some(DdUrl::new("http://a_dd_url".to_string()).unwrap()),
                None
            ),
            Some(MetricsIntakeUrlPrefixOverride(
                "http://a_dd_url".to_string()
            ))
        );
    }

    #[test]
    fn test_intake_url_prefix_new_requires_something() {
        assert_eq!(
            MetricsIntakeUrlPrefix::new(None, None),
            Err(MissingIntakeUrlError)
        );
    }

    #[test]
    fn test_intake_url_prefix_new_picks_the_override() {
        assert_eq!(
            MetricsIntakeUrlPrefix::new(
                Some(Site::new("a_site".to_string()).unwrap()),
                MetricsIntakeUrlPrefixOverride::maybe_new(
                    Some(DdUrl::new("http://a_dd_url".to_string()).unwrap()),
                    None
                ),
            ),
            Ok(MetricsIntakeUrlPrefix::new_expect_validated(
                "http://a_dd_url".to_string()
            ))
        );
    }

    #[test]
    fn test_intake_url_prefix_new_picks_site_as_a_fallback() {
        assert_eq!(
            MetricsIntakeUrlPrefix::new(Some(Site::new("a_site".to_string()).unwrap()), None,),
            Ok(MetricsIntakeUrlPrefix::new_expect_validated(
                "https://api.a_site".to_string()
            ))
        );
    }
}
