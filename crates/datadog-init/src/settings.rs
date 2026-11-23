// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Settings that several components read the same way.

use datadog_serverless_core::env::parse_bool;

const DEFAULT_SITE: &str = "datadoghq.com";

/// Go's well-known Datadog domains. Go matches them at the end of the site, without a label
/// boundary.
const KNOWN_DATADOG_DOMAINS: [&str; 5] = [
    "datadoghq.com",
    "datadoghq.eu",
    "datad0g.com",
    "datad0g.eu",
    "ddog-gov.com",
];

/// `DD_SITE`, without surrounding whitespace. Unset, empty or blank gives `datadoghq.com`, as in
/// the Go agent.
pub fn site(lookup: impl Fn(&str) -> Option<String>) -> String {
    lookup("DD_SITE")
        .map(|site| site.trim().to_string())
        .filter(|site| !site.is_empty())
        .unwrap_or_else(|| DEFAULT_SITE.to_string())
}

/// The site for the intake URLs that `datadog-init` builds from the site. Like Go's
/// `BuildURLWithPrefix`, a site that ends with a known Datadog domain gets a trailing dot, so the
/// host is a fully qualified domain name, unless `DD_CONVERT_DD_SITE_FQDN_ENABLED` is false.
/// Intake URLs that the user sets are not changed.
pub fn intake_site(lookup: impl Fn(&str) -> Option<String>) -> String {
    // Go reads an invalid value as false and ignores an empty one.
    let enabled = lookup("DD_CONVERT_DD_SITE_FQDN_ENABLED")
        .filter(|value| !value.is_empty())
        .is_none_or(|value| parse_bool(Some(&value), true).unwrap_or(false));
    // Go's IDNA normalization lowercases the site.
    let site = site(&lookup).to_lowercase();
    let known = KNOWN_DATADOG_DOMAINS
        .iter()
        .any(|domain| site.ends_with(domain));
    if enabled && known {
        format!("{site}.")
    } else {
        site
    }
}

/// The first non-empty of `DD_API_KEY` and `DD_APM_API_KEY`, without surrounding whitespace, as
/// the trace agent reads it.
pub fn api_key(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["DD_API_KEY", "DD_APM_API_KEY"]
        .into_iter()
        .filter_map(lookup)
        .map(|key| key.trim().to_string())
        .find(|key| !key.is_empty())
}

/// `DD_PROXY_HTTPS`, else `HTTPS_PROXY`.
pub fn proxy_url(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    lookup("DD_PROXY_HTTPS").or_else(|| lookup("HTTPS_PROXY"))
}

/// `DD_HEALTH_PORT`, the port of the health endpoint. Unset, unparsable, zero or negative turns
/// the endpoint off, as Go reads `health_port`.
pub fn health_port(lookup: impl Fn(&str) -> Option<String>) -> Option<i64> {
    lookup("DD_HEALTH_PORT")
        .and_then(|port| port.parse::<i64>().ok())
        .filter(|port| *port > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_in<'a>(variables: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        |name| {
            variables
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn site_table() {
        let cases: &[(&[(&str, &str)], &str)] = &[
            (&[], "datadoghq.com"),
            (&[("DD_SITE", "datadoghq.eu")], "datadoghq.eu"),
            (&[("DD_SITE", " datadoghq.eu \n")], "datadoghq.eu"),
            (&[("DD_SITE", "")], "datadoghq.com"),
            (&[("DD_SITE", "   ")], "datadoghq.com"),
        ];
        for (variables, expected) in cases {
            assert_eq!(
                site(lookup_in(variables)),
                *expected,
                "variables {variables:?}"
            );
        }
    }

    #[test]
    fn intake_site_table() {
        let cases: &[(&[(&str, &str)], &str)] = &[
            (&[], "datadoghq.com."),
            (&[("DD_SITE", "datadoghq.eu")], "datadoghq.eu."),
            (&[("DD_SITE", "us3.datadoghq.com")], "us3.datadoghq.com."),
            (&[("DD_SITE", "datad0g.com")], "datad0g.com."),
            (&[("DD_SITE", "datad0g.eu")], "datad0g.eu."),
            (&[("DD_SITE", "ddog-gov.com")], "ddog-gov.com."),
            (&[("DD_SITE", " datadoghq.eu \n")], "datadoghq.eu."),
            (&[("DD_SITE", "datadoghq.com.")], "datadoghq.com."),
            (&[("DD_SITE", "US3.DATADOGHQ.COM")], "us3.datadoghq.com."),
            (&[("DD_SITE", "example.com")], "example.com"),
            (&[("DD_SITE", "fakeintake:80")], "fakeintake:80"),
            (
                &[("DD_CONVERT_DD_SITE_FQDN_ENABLED", "false")],
                "datadoghq.com",
            ),
            (
                &[("DD_CONVERT_DD_SITE_FQDN_ENABLED", "invalid")],
                "datadoghq.com",
            ),
            (&[("DD_CONVERT_DD_SITE_FQDN_ENABLED", "")], "datadoghq.com."),
            (
                &[("DD_CONVERT_DD_SITE_FQDN_ENABLED", "true")],
                "datadoghq.com.",
            ),
        ];
        for (variables, expected) in cases {
            assert_eq!(
                intake_site(lookup_in(variables)),
                *expected,
                "variables {variables:?}"
            );
        }
    }

    #[test]
    fn health_port_table() {
        type Case = (&'static [(&'static str, &'static str)], Option<i64>);
        let cases: &[Case] = &[
            (&[], None),
            (&[("DD_HEALTH_PORT", "12345")], Some(12345)),
            (&[("DD_HEALTH_PORT", "70000")], Some(70000)),
            (&[("DD_HEALTH_PORT", "0")], None),
            (&[("DD_HEALTH_PORT", "-1")], None),
            (&[("DD_HEALTH_PORT", "")], None),
            (&[("DD_HEALTH_PORT", "port")], None),
        ];
        for (variables, expected) in cases {
            assert_eq!(
                health_port(lookup_in(variables)),
                *expected,
                "variables {variables:?}"
            );
        }
    }

    #[test]
    fn proxy_url_table() {
        type Case = (
            &'static [(&'static str, &'static str)],
            Option<&'static str>,
        );
        let cases: &[Case] = &[
            (&[], None),
            (
                &[("HTTPS_PROXY", "http://proxy:3128")],
                Some("http://proxy:3128"),
            ),
            (
                &[
                    ("DD_PROXY_HTTPS", "http://datadog-proxy:3128"),
                    ("HTTPS_PROXY", "http://proxy:3128"),
                ],
                Some("http://datadog-proxy:3128"),
            ),
        ];
        for (variables, expected) in cases {
            assert_eq!(
                proxy_url(lookup_in(variables)).as_deref(),
                *expected,
                "variables {variables:?}"
            );
        }
    }
}
