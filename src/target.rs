#![allow(dead_code)] // Cluster selectors are consumed by Milestone 1 CLI wiring.

#[cfg(test)]
mod tests {
    use super::{ClusterSelectorError, parse_cluster_selector};

    const IDENTIFIER: &str = "0123456789abcdefghijklmnop";

    #[test]
    fn parses_each_supported_cluster_selector_into_application_owned_fields() {
        struct Case {
            selector: String,
            identifier: &'static str,
            region: Option<&'static str>,
            account_id: Option<&'static str>,
            partition: Option<&'static str>,
            arn: Option<String>,
            endpoint: Option<String>,
        }

        let arn = format!("arn:aws:dsql:us-east-1:123456789012:cluster/{IDENTIFIER}");
        let endpoint = format!("{IDENTIFIER}.dsql.us-east-1.on.aws");
        let cases = [
            Case {
                selector: IDENTIFIER.into(),
                identifier: IDENTIFIER,
                region: None,
                account_id: None,
                partition: None,
                arn: None,
                endpoint: None,
            },
            Case {
                selector: arn.clone(),
                identifier: IDENTIFIER,
                region: Some("us-east-1"),
                account_id: Some("123456789012"),
                partition: Some("aws"),
                arn: Some(arn),
                endpoint: None,
            },
            Case {
                selector: endpoint.clone(),
                identifier: IDENTIFIER,
                region: Some("us-east-1"),
                account_id: None,
                partition: None,
                arn: None,
                endpoint: Some(endpoint),
            },
        ];

        for case in cases {
            let selector = parse_cluster_selector(&case.selector).expect("valid cluster selector");

            assert_eq!(selector.identifier(), case.identifier);
            assert_eq!(selector.region(), case.region);
            assert_eq!(selector.account_id(), case.account_id);
            assert_eq!(selector.partition(), case.partition);
            assert_eq!(selector.arn(), case.arn.as_deref());
            assert_eq!(selector.endpoint(), case.endpoint.as_deref());
        }
    }

    #[test]
    fn accepts_known_aws_partitions_with_matching_region_prefixes() {
        let cases = [
            ("aws", "us-east-1"),
            ("aws-cn", "cn-north-1"),
            ("aws-us-gov", "us-gov-west-1"),
            ("aws-iso", "us-iso-east-1"),
            ("aws-iso-b", "us-isob-east-1"),
            ("aws-iso-e", "eu-isoe-west-1"),
            ("aws-iso-f", "us-isof-south-1"),
        ];

        for (partition, region) in cases {
            let arn = format!("arn:{partition}:dsql:{region}:123456789012:cluster/{IDENTIFIER}");
            let selector = parse_cluster_selector(&arn).expect("valid partition and Region");

            assert_eq!(selector.partition(), Some(partition));
            assert_eq!(selector.region(), Some(region));
        }
    }

    #[test]
    fn rejects_malformed_and_ambiguous_cluster_selectors() {
        let valid_arn = format!("arn:aws:dsql:us-east-1:123456789012:cluster/{IDENTIFIER}");
        let valid_endpoint = format!("{IDENTIFIER}.dsql.us-east-1.on.aws");
        let malformed = [
            "",
            " 0123456789abcdefghijklmnop",
            "0123456789abcdefghijklmnop ",
            "0123456789abcdefghijklmno",  // identifier is too short
            "0123456789abcdefghijklmnoP", // uppercase identifier
            "arn:aws:dsql:us-east-1:12345678901:cluster/0123456789abcdefghijklmnop",
            "arn:aws:dsql:us-east-1:1234567890123:cluster/0123456789abcdefghijklmnop",
            "arn:gcp:dsql:us-east-1:123456789012:cluster/0123456789abcdefghijklmnop",
            "arn:aws:dsql:us-east-1:123456789012:stream/0123456789abcdefghijklmnop",
            "arn:aws:rds:us-east-1:123456789012:cluster/0123456789abcdefghijklmnop",
            "arn:aws:dsql:us-east:123456789012:cluster/0123456789abcdefghijklmnop",
            "arn:aws:dsql:US-east-1:123456789012:cluster/0123456789abcdefghijklmnop",
            "arn:aws-us-gov:dsql:us-east-1:123456789012:cluster/0123456789abcdefghijklmnop",
            "arn:aws:dsql:us-east-1:123456789012:cluster/0123456789abcdefghijklmnop/extra",
            "0123456789abcdefghijklmnop.dsql.us-east-1.amazonaws.com",
            "0123456789abcdefghijklmnop.dsql.us-east-1.on.aws.",
            "0123456789abcdefghijklmnop.dsql.us-east.on.aws",
            "0123456789abcdefghijklmnop.dsql.US-east-1.on.aws",
            "0123456789abcdefghijklmnop.dsql.us-east-1.on.aws:5432",
            "https://0123456789abcdefghijklmnop.dsql.us-east-1.on.aws",
            "bad.dsql.us-east-1.on.aws",
        ];

        assert!(parse_cluster_selector(&valid_arn).is_ok());
        assert!(parse_cluster_selector(&valid_endpoint).is_ok());
        for selector in malformed {
            assert!(
                matches!(
                    parse_cluster_selector(selector),
                    Err(ClusterSelectorError::Malformed { .. })
                ),
                "accepted malformed cluster selector: {selector}"
            );
        }
    }

    #[test]
    fn rejects_a_resolved_region_that_conflicts_with_an_encoded_region() {
        let arn = format!("arn:aws:dsql:us-east-1:123456789012:cluster/{IDENTIFIER}");
        let endpoint = format!("{IDENTIFIER}.dsql.us-east-1.on.aws");
        let bare = parse_cluster_selector(IDENTIFIER).expect("valid bare identifier");
        let encoded = parse_cluster_selector(&arn).expect("valid cluster ARN");
        let endpoint = parse_cluster_selector(&endpoint).expect("valid canonical endpoint");

        assert_eq!(bare.check_region("eu-west-1"), Ok(()));
        assert!(matches!(
            encoded.check_region("eu-west-1"),
            Err(ClusterSelectorError::RegionConflict { .. })
        ));
        assert!(matches!(
            endpoint.check_region("eu-west-1"),
            Err(ClusterSelectorError::RegionConflict { .. })
        ));
        assert!(matches!(
            encoded.check_region("not a region"),
            Err(ClusterSelectorError::MalformedResolvedRegion { .. })
        ));
    }

    #[test]
    fn malformed_input_property_coverage_rejects_invalid_ascii_mutations_and_structural_changes() {
        let valid = [
            IDENTIFIER.to_owned(),
            format!("arn:aws:dsql:us-east-1:123456789012:cluster/{IDENTIFIER}"),
            format!("{IDENTIFIER}.dsql.us-east-1.on.aws"),
        ];

        for selector in valid {
            for index in 0..selector.len() {
                for invalid_ascii in [b'!', b' ', b'/', b'@', b'Z'] {
                    if selector.as_bytes()[index] == invalid_ascii {
                        continue;
                    }
                    let mut mutated = selector.clone().into_bytes();
                    mutated[index] = invalid_ascii;
                    let mutated = String::from_utf8(mutated).expect("ASCII mutation");

                    assert!(
                        parse_cluster_selector(&mutated).is_err(),
                        "accepted invalid ASCII mutation at index {index}: {mutated}"
                    );
                }

                if !selector.as_bytes()[index].is_ascii_lowercase()
                    && !selector.as_bytes()[index].is_ascii_digit()
                {
                    let mut truncated = selector.clone().into_bytes();
                    truncated.remove(index);
                    let truncated = String::from_utf8(truncated).expect("ASCII truncation");
                    assert!(
                        parse_cluster_selector(&truncated).is_err(),
                        "accepted structural truncation at index {index}: {truncated}"
                    );
                }
            }
            for extended in [format!("x{selector}"), format!("{selector}x")] {
                assert!(
                    parse_cluster_selector(&extended).is_err(),
                    "accepted structural extension: {extended}"
                );
            }
        }
    }
}
use std::{error::Error, fmt};

/// An application-owned, validated cluster selector. ARN and endpoint fields
/// are populated only when that form was supplied, so later adapters can use
/// their available context without reparsing user input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClusterSelector {
    identifier: String,
    region: Option<String>,
    account_id: Option<String>,
    partition: Option<String>,
    arn: Option<String>,
    endpoint: Option<String>,
}

impl ClusterSelector {
    pub(crate) fn identifier(&self) -> &str {
        &self.identifier
    }

    pub(crate) fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    pub(crate) fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub(crate) fn partition(&self) -> Option<&str> {
        self.partition.as_deref()
    }

    pub(crate) fn arn(&self) -> Option<&str> {
        self.arn.as_deref()
    }

    pub(crate) fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Reject a resolved Region that conflicts with an ARN or canonical
    /// endpoint. A bare cluster ID does not encode a Region.
    pub(crate) fn check_region(&self, resolved_region: &str) -> Result<(), ClusterSelectorError> {
        if !is_region(resolved_region) {
            return Err(ClusterSelectorError::MalformedResolvedRegion {
                region: resolved_region.into(),
            });
        }

        if let Some(selector_region) = self.region()
            && selector_region != resolved_region
        {
            return Err(ClusterSelectorError::RegionConflict {
                selector_region: selector_region.into(),
                resolved_region: resolved_region.into(),
            });
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ClusterSelectorError {
    Malformed {
        selector: String,
    },
    MalformedResolvedRegion {
        region: String,
    },
    RegionConflict {
        selector_region: String,
        resolved_region: String,
    },
}

impl fmt::Display for ClusterSelectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { .. } => formatter.write_str(
                "cluster selector must be a cluster ID, DSQL cluster ARN, or canonical DSQL endpoint",
            ),
            Self::MalformedResolvedRegion { .. } => {
                formatter.write_str("resolved Region has invalid syntax")
            }
            Self::RegionConflict { .. } => {
                formatter.write_str("cluster selector Region conflicts with the resolved Region")
            }
        }
    }
}

impl Error for ClusterSelectorError {}

/// Parse a cluster selector without making a network call.
pub(crate) fn parse_cluster_selector(input: &str) -> Result<ClusterSelector, ClusterSelectorError> {
    if let Some(selector) = parse_arn(input) {
        return selector;
    }
    if let Some(selector) = parse_endpoint(input) {
        return selector;
    }
    if is_identifier(input) {
        return Ok(ClusterSelector {
            identifier: input.into(),
            region: None,
            account_id: None,
            partition: None,
            arn: None,
            endpoint: None,
        });
    }

    Err(malformed(input))
}

fn parse_arn(input: &str) -> Option<Result<ClusterSelector, ClusterSelectorError>> {
    if !input.starts_with("arn:") {
        return None;
    }

    let parts: Vec<_> = input.split(':').collect();
    let ["arn", partition, "dsql", region, account_id, resource] = parts.as_slice() else {
        return Some(Err(malformed(input)));
    };
    let Some(identifier) = resource.strip_prefix("cluster/") else {
        return Some(Err(malformed(input)));
    };

    if !is_partition(partition)
        || !is_region(region)
        || !partition_matches_region(partition, region)
        || !is_account_id(account_id)
        || !is_identifier(identifier)
    {
        return Some(Err(malformed(input)));
    }

    Some(Ok(ClusterSelector {
        identifier: (*identifier).into(),
        region: Some((*region).into()),
        account_id: Some((*account_id).into()),
        partition: Some((*partition).into()),
        arn: Some(input.into()),
        endpoint: None,
    }))
}

fn parse_endpoint(input: &str) -> Option<Result<ClusterSelector, ClusterSelectorError>> {
    if !input.contains('.') {
        return None;
    }

    let labels: Vec<_> = input.split('.').collect();
    let [identifier, "dsql", region, "on", "aws"] = labels.as_slice() else {
        return Some(Err(malformed(input)));
    };
    if !is_identifier(identifier) || !is_region(region) {
        return Some(Err(malformed(input)));
    }

    Some(Ok(ClusterSelector {
        identifier: (*identifier).into(),
        region: Some((*region).into()),
        account_id: None,
        partition: None,
        arn: None,
        endpoint: Some(input.into()),
    }))
}

fn malformed(selector: &str) -> ClusterSelectorError {
    ClusterSelectorError::Malformed {
        selector: selector.into(),
    }
}

fn is_identifier(value: &str) -> bool {
    value.len() == 26
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn is_account_id(value: &str) -> bool {
    value.len() == 12 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_partition(value: &str) -> bool {
    matches!(
        value,
        "aws" | "aws-cn" | "aws-us-gov" | "aws-iso" | "aws-iso-b" | "aws-iso-e" | "aws-iso-f"
    )
}

fn partition_matches_region(partition: &str, region: &str) -> bool {
    match partition {
        "aws" => {
            !region.starts_with("cn-")
                && !region.starts_with("us-gov-")
                && !region.starts_with("us-iso-")
                && !region.starts_with("us-isob-")
                && !region.starts_with("eu-isoe-")
                && !region.starts_with("us-isof-")
        }
        "aws-cn" => region.starts_with("cn-"),
        "aws-us-gov" => region.starts_with("us-gov-"),
        "aws-iso" => region.starts_with("us-iso-"),
        "aws-iso-b" => region.starts_with("us-isob-"),
        "aws-iso-e" => region.starts_with("eu-isoe-"),
        "aws-iso-f" => region.starts_with("us-isof-"),
        _ => false,
    }
}

pub(crate) fn is_region(value: &str) -> bool {
    if value.is_empty() || value.len() > 20 {
        return false;
    }

    let parts: Vec<_> = value.split('-').collect();
    if parts.len() < 3 {
        return false;
    }

    for part in &parts[..parts.len() - 1] {
        if part.is_empty()
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            return false;
        }
    }

    parts
        .last()
        .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}
