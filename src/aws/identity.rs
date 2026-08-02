#![allow(dead_code)] // Consumed when the integration owner wires AWS-003 into startup.

use crate::{app::CallerIdentity, aws::config::AwsConfiguration};
use aws_config::SdkConfig;
use std::{future::Future, pin::Pin};

/// The application-owned portion of an STS `GetCallerIdentity` response.
/// Generated SDK output remains in this adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StsCallerIdentityResponse {
    account_id: Option<String>,
    arn: Option<String>,
}

impl StsCallerIdentityResponse {
    pub(crate) fn new(account_id: Option<String>, arn: Option<String>) -> Self {
        Self { account_id, arn }
    }
}

/// Stable, diagnostic-safe reasons that caller identity could not be resolved.
/// These deliberately retain neither SDK errors nor their potentially sensitive
/// request context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CallerIdentityFailure {
    AccessDenied,
    CredentialsUnavailable,
    EndpointUnavailable,
    RequestFailed,
}

impl CallerIdentityFailure {
    pub(crate) const fn diagnostic(self) -> &'static str {
        match self {
            Self::AccessDenied => "could not resolve AWS caller identity: access denied",
            Self::CredentialsUnavailable => {
                "could not resolve AWS caller identity: credentials unavailable"
            }
            Self::EndpointUnavailable => {
                "could not resolve AWS caller identity: STS endpoint unavailable"
            }
            Self::RequestFailed => "could not resolve AWS caller identity",
        }
    }
}

/// A successful lookup always has a `CallerIdentity`, even when STS omitted one
/// or both optional fields. A failed lookup is a warning, not an application
/// error, so discovery and direct connection can continue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CallerIdentityLookup {
    identity: Option<CallerIdentity>,
    warning: Option<CallerIdentityFailure>,
}

impl CallerIdentityLookup {
    fn resolved(identity: CallerIdentity) -> Self {
        Self {
            identity: Some(identity),
            warning: None,
        }
    }

    fn unavailable(warning: CallerIdentityFailure) -> Self {
        Self {
            identity: None,
            warning: Some(warning),
        }
    }

    pub(crate) fn identity(&self) -> Option<&CallerIdentity> {
        self.identity.as_ref()
    }

    pub(crate) fn warning(&self) -> Option<CallerIdentityFailure> {
        self.warning
    }

    #[cfg(test)]
    pub(crate) fn test_unavailable(warning: CallerIdentityFailure) -> Self {
        Self::unavailable(warning)
    }

    #[cfg(test)]
    pub(crate) fn test_resolved(identity: CallerIdentity) -> Self {
        Self::resolved(identity)
    }
}

pub(crate) type CallerIdentityFuture<'a> = Pin<
    Box<dyn Future<Output = Result<StsCallerIdentityResponse, CallerIdentityFailure>> + Send + 'a>,
>;

/// Minimal async seam for deterministic callers and tests. It exposes only
/// application-owned values and stable failure categories.
pub(crate) trait CallerIdentityClient {
    fn get_caller_identity(&self) -> CallerIdentityFuture<'_>;
}

/// Resolves caller context exactly once. Failure is represented as a safe
/// warning rather than an error so this optional display context never blocks
/// cluster discovery or direct connection.
pub(crate) async fn resolve_caller_identity(
    client: &dyn CallerIdentityClient,
) -> CallerIdentityLookup {
    match client.get_caller_identity().await {
        Ok(response) => {
            CallerIdentityLookup::resolved(CallerIdentity::new(response.account_id, response.arn))
        }
        Err(failure) => CallerIdentityLookup::unavailable(failure),
    }
}

/// Production entry point. Keeping SDK configuration access here prevents an
/// STS client or its generated types from escaping the AWS adapter layer.
pub(crate) async fn resolve_aws_caller_identity(
    configuration: &AwsConfiguration,
) -> CallerIdentityLookup {
    let client = AwsStsCallerIdentity::new(configuration.sdk_config());
    resolve_caller_identity(&client).await
}

/// The real STS adapter. It owns the generated client and converts its output
/// and errors before they leave `src/aws`.
pub(crate) struct AwsStsCallerIdentity {
    client: aws_sdk_sts::Client,
}

impl AwsStsCallerIdentity {
    pub(crate) fn new(config: &SdkConfig) -> Self {
        Self {
            client: aws_sdk_sts::Client::new(config),
        }
    }
}

impl CallerIdentityClient for AwsStsCallerIdentity {
    fn get_caller_identity(&self) -> CallerIdentityFuture<'_> {
        Box::pin(async move {
            self.client
                .get_caller_identity()
                .send()
                .await
                .map(|output| {
                    StsCallerIdentityResponse::new(
                        output.account().map(str::to_owned),
                        output.arn().map(str::to_owned),
                    )
                })
                .map_err(classify_sdk_error)
        })
    }
}

fn classify_sdk_error(
    error: aws_sdk_sts::error::SdkError<
        aws_sdk_sts::operation::get_caller_identity::GetCallerIdentityError,
    >,
) -> CallerIdentityFailure {
    use aws_sdk_sts::error::{ProvideErrorMetadata, SdkError};

    match error {
        SdkError::ConstructionFailure(_) => CallerIdentityFailure::CredentialsUnavailable,
        SdkError::DispatchFailure(_) | SdkError::ResponseError(_) | SdkError::TimeoutError(_) => {
            CallerIdentityFailure::EndpointUnavailable
        }
        SdkError::ServiceError(service) => match service.err().code() {
            Some("AccessDenied" | "AccessDeniedException" | "UnauthorizedOperation") => {
                CallerIdentityFailure::AccessDenied
            }
            Some(
                "ExpiredToken"
                | "ExpiredTokenException"
                | "InvalidClientTokenId"
                | "UnrecognizedClientException",
            ) => CallerIdentityFailure::CredentialsUnavailable,
            _ => CallerIdentityFailure::RequestFailed,
        },
        _ => CallerIdentityFailure::RequestFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CallerIdentityClient, CallerIdentityFailure, CallerIdentityFuture,
        StsCallerIdentityResponse, resolve_caller_identity,
    };
    use std::sync::{Arc, Mutex};

    struct FakeSts {
        response: Result<StsCallerIdentityResponse, CallerIdentityFailure>,
        calls: Arc<Mutex<usize>>,
    }

    impl CallerIdentityClient for FakeSts {
        fn get_caller_identity(&self) -> CallerIdentityFuture<'_> {
            let calls = Arc::clone(&self.calls);
            let response = self.response.clone();
            Box::pin(async move {
                *calls.lock().expect("call count lock") += 1;
                response
            })
        }
    }

    fn fake(response: Result<StsCallerIdentityResponse, CallerIdentityFailure>) -> FakeSts {
        FakeSts {
            response,
            calls: Arc::new(Mutex::new(0)),
        }
    }

    #[tokio::test]
    async fn calls_sts_once_and_maps_account_and_arn() {
        let sts = fake(Ok(StsCallerIdentityResponse::new(
            Some("123456789012".into()),
            Some("arn:aws:iam::123456789012:role/discovery".into()),
        )));

        let lookup = resolve_caller_identity(&sts).await;

        assert_eq!(*sts.calls.lock().expect("call count lock"), 1);
        assert_eq!(lookup.warning(), None);
        let identity = lookup.identity().expect("successful identity");
        assert_eq!(identity.account_id(), Some("123456789012"));
        assert_eq!(
            identity.principal(),
            Some("arn:aws:iam::123456789012:role/discovery")
        );
    }

    #[tokio::test]
    async fn preserves_partial_or_missing_sts_fields() {
        for (account, arn) in [
            (Some("123456789012"), None),
            (None, Some("arn:aws:iam::123456789012:user/alice")),
            (None, None),
        ] {
            let sts = fake(Ok(StsCallerIdentityResponse::new(
                account.map(str::to_owned),
                arn.map(str::to_owned),
            )));

            let lookup = resolve_caller_identity(&sts).await;

            assert_eq!(lookup.warning(), None);
            let identity = lookup.identity().expect("successful identity");
            assert_eq!(identity.account_id(), account);
            assert_eq!(identity.principal(), arn);
        }
    }

    #[tokio::test]
    async fn failures_warn_and_do_not_block_continuation() {
        for failure in [
            CallerIdentityFailure::AccessDenied,
            CallerIdentityFailure::EndpointUnavailable,
            CallerIdentityFailure::CredentialsUnavailable,
        ] {
            let sts = fake(Err(failure));

            let lookup = resolve_caller_identity(&sts).await;
            let discovery_continues = lookup.identity().is_none();

            assert_eq!(*sts.calls.lock().expect("call count lock"), 1);
            assert_eq!(lookup.warning(), Some(failure));
            assert!(discovery_continues);
        }
    }

    #[test]
    fn warnings_are_stable_and_do_not_include_secrets() {
        for failure in [
            CallerIdentityFailure::AccessDenied,
            CallerIdentityFailure::EndpointUnavailable,
            CallerIdentityFailure::CredentialsUnavailable,
            CallerIdentityFailure::RequestFailed,
        ] {
            let diagnostic = failure.diagnostic();
            assert!(!diagnostic.contains("AKIA"));
            assert!(!diagnostic.contains("token="));
            assert!(!diagnostic.contains("signature="));
        }
    }
}
