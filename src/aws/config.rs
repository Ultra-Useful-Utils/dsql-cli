use crate::{app::ResolvedAwsContext, error::ApplicationError, target::is_region};
use aws_config::{BehaviorVersion, SdkConfig, meta::region::RegionProviderChain};
use aws_types::region::Region;

/// The non-secret source selected by Region resolution. This is intended for
/// verbose diagnostics; it never includes credential-provider output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegionResolutionSource {
    ExplicitFlag,
    Selector,
    SdkProvider,
    InteractivePrompt,
}

/// Application-safe details of the Region decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegionResolution {
    region: String,
    source: RegionResolutionSource,
}

impl RegionResolution {
    fn new(region: String, source: RegionResolutionSource) -> Self {
        Self { region, source }
    }

    pub(crate) fn region(&self) -> &str {
        &self.region
    }

    pub(crate) fn source(&self) -> RegionResolutionSource {
        self.source
    }
}

/// The narrow interactive seam used only after command and SDK Region sources
/// have been exhausted.
pub(crate) trait RegionPrompt {
    fn prompt_region(&mut self) -> Result<Option<String>, ApplicationError>;
}

/// Inputs used by the deterministic Region resolver. `sdk_region` is supplied
/// from the loaded SDK config in production and directly by unit tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegionResolutionRequest {
    explicit_region: Option<String>,
    selector_region: Option<String>,
    sdk_region: Option<String>,
    interactive: bool,
}

impl RegionResolutionRequest {
    pub(crate) fn new(
        explicit_region: Option<&str>,
        selector_region: Option<&str>,
        sdk_region: Option<&str>,
        interactive: bool,
    ) -> Self {
        Self {
            explicit_region: explicit_region.map(str::to_owned),
            selector_region: selector_region.map(str::to_owned),
            sdk_region: sdk_region.map(str::to_owned),
            interactive,
        }
    }
}

/// Resolves a Region without loading credentials or contacting AWS.
pub(crate) fn resolve_region(
    request: RegionResolutionRequest,
    prompt: &mut dyn RegionPrompt,
) -> Result<RegionResolution, ApplicationError> {
    let explicit_region = required_region(request.explicit_region, "--region")?;
    let selector_region = required_region(request.selector_region, "cluster selector")?;
    let sdk_region = required_region(request.sdk_region, "AWS SDK configuration")?;

    if let (Some(explicit_region), Some(selector_region)) =
        (explicit_region.as_deref(), selector_region.as_deref())
        && explicit_region != selector_region
    {
        return Err(ApplicationError::usage(
            "--region conflicts with the Region encoded in the cluster selector",
        ));
    }

    if let Some(region) = explicit_region {
        return Ok(RegionResolution::new(
            region,
            RegionResolutionSource::ExplicitFlag,
        ));
    }
    if let Some(region) = selector_region {
        return Ok(RegionResolution::new(
            region,
            RegionResolutionSource::Selector,
        ));
    }
    if let Some(region) = sdk_region {
        return Ok(RegionResolution::new(
            region,
            RegionResolutionSource::SdkProvider,
        ));
    }
    if request.interactive {
        let prompted_region = prompt.prompt_region()?;
        if let Some(region) = required_region(prompted_region, "interactive prompt")? {
            return Ok(RegionResolution::new(
                region,
                RegionResolutionSource::InteractivePrompt,
            ));
        }
    }

    Err(ApplicationError::usage(
        "Region is required; pass --region, use a cluster ARN or canonical endpoint, or configure an AWS SDK Region",
    ))
}

fn required_region(
    region: Option<String>,
    source: &str,
) -> Result<Option<String>, ApplicationError> {
    match region {
        Some(region) if region.trim().is_empty() => Err(ApplicationError::usage(format!(
            "{source} must not be empty"
        ))),
        Some(region) if !is_region(&region) => Err(ApplicationError::usage(format!(
            "{source} has invalid Region syntax"
        ))),
        Some(region) => Ok(Some(region)),
        None => Ok(None),
    }
}

/// AWS-specific inputs collected by CLI/target parsing. The target parser owns
/// ARN and endpoint validation and passes only its inferred Region here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AwsConfigRequest {
    profile_name: Option<String>,
    explicit_region: Option<String>,
    selector_region: Option<String>,
    interactive: bool,
}

impl AwsConfigRequest {
    pub(crate) fn new(
        profile_name: Option<String>,
        explicit_region: Option<String>,
        selector_region: Option<String>,
        interactive: bool,
    ) -> Self {
        Self {
            profile_name,
            explicit_region,
            selector_region,
            interactive,
        }
    }
}

/// Verbose-safe Region diagnostics. The profile label is user-supplied metadata,
/// not credential-provider output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegionDiagnostics {
    source: RegionResolutionSource,
    profile_label: Option<String>,
}

impl RegionDiagnostics {
    pub(crate) fn source(&self) -> RegionResolutionSource {
        self.source
    }

    pub(crate) fn profile_label(&self) -> Option<&str> {
        self.profile_label.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn test_new(source: RegionResolutionSource, profile_label: Option<String>) -> Self {
        Self {
            source,
            profile_label,
        }
    }
}

/// Keeps the generated SDK configuration inside the AWS adapter while exposing
/// only application-owned context and redacted-safe decision diagnostics.
pub(crate) struct AwsConfiguration {
    sdk_config: SdkConfig,
    context: ResolvedAwsContext,
    region_diagnostics: RegionDiagnostics,
}

impl AwsConfiguration {
    pub(crate) fn context(&self) -> &ResolvedAwsContext {
        &self.context
    }

    pub(crate) fn region_diagnostics(&self) -> &RegionDiagnostics {
        &self.region_diagnostics
    }

    pub(crate) fn sdk_config(&self) -> &SdkConfig {
        &self.sdk_config
    }
}

/// Loads the AWS SDK default configuration without making an AWS service call.
/// Explicit and selector Regions are injected ahead of the SDK's default
/// provider chain. A prompted Region reloads the SDK config so AWS clients and
/// application context always agree.
pub(crate) async fn load_aws_configuration(
    request: AwsConfigRequest,
    prompt: &mut dyn RegionPrompt,
) -> Result<AwsConfiguration, ApplicationError> {
    let explicit_region = required_region(request.explicit_region.clone(), "--region")?;
    let selector_region = required_region(request.selector_region.clone(), "cluster selector")?;
    if let (Some(explicit_region), Some(selector_region)) =
        (explicit_region.as_deref(), selector_region.as_deref())
        && explicit_region != selector_region
    {
        return Err(ApplicationError::usage(
            "--region conflicts with the Region encoded in the cluster selector",
        ));
    }

    let preferred_region = explicit_region.clone().or(selector_region.clone());
    let mut sdk_config = load_sdk_config(&request, preferred_region).await;
    let mut resolution = resolve_region(
        RegionResolutionRequest::new(
            explicit_region.as_deref(),
            selector_region.as_deref(),
            sdk_config.region().map(|region| region.as_ref()),
            request.interactive,
        ),
        prompt,
    )?;

    if resolution.source() == RegionResolutionSource::InteractivePrompt {
        sdk_config = load_sdk_config(&request, Some(resolution.region().to_owned())).await;
        resolution = RegionResolution::new(
            resolution.region().to_owned(),
            RegionResolutionSource::InteractivePrompt,
        );
    }

    let profile_label = request.profile_name.clone();
    Ok(AwsConfiguration {
        sdk_config,
        context: ResolvedAwsContext::new(resolution.region(), profile_label.clone(), None),
        region_diagnostics: RegionDiagnostics {
            source: resolution.source(),
            profile_label,
        },
    })
}

async fn load_sdk_config(
    request: &AwsConfigRequest,
    preferred_region: Option<String>,
) -> SdkConfig {
    let region_provider = match preferred_region {
        Some(region) => RegionProviderChain::first_try(Region::new(region)).or_default_provider(),
        None => RegionProviderChain::default_provider(),
    };
    let mut loader = aws_config::defaults(BehaviorVersion::latest()).region(region_provider);
    if let Some(profile_name) = request.profile_name.as_deref() {
        loader = loader.profile_name(profile_name);
    }
    loader.load().await
}

#[cfg(test)]
mod tests {
    use super::{RegionPrompt, RegionResolutionRequest, RegionResolutionSource, resolve_region};
    use crate::error::ErrorCategory;

    struct FakePrompt {
        response: Option<&'static str>,
        calls: usize,
    }

    impl RegionPrompt for FakePrompt {
        fn prompt_region(&mut self) -> Result<Option<String>, crate::error::ApplicationError> {
            self.calls += 1;
            Ok(self.response.map(str::to_owned))
        }
    }

    #[test]
    fn explicit_region_wins_over_the_sdk_provider_without_prompting() {
        let mut prompt = FakePrompt {
            response: Some("eu-west-1"),
            calls: 0,
        };

        let resolved = resolve_region(
            RegionResolutionRequest::new(Some("us-east-1"), None, Some("eu-west-1"), true),
            &mut prompt,
        )
        .expect("explicit Region resolves");

        assert_eq!(resolved.region(), "us-east-1");
        assert_eq!(resolved.source(), RegionResolutionSource::ExplicitFlag);
        assert_eq!(prompt.calls, 0);
    }

    #[test]
    fn arn_and_endpoint_inferred_regions_win_over_the_sdk_provider_without_prompting() {
        for (selector_kind, selector_region) in
            [("ARN", "us-west-2"), ("canonical endpoint", "eu-central-1")]
        {
            let mut prompt = FakePrompt {
                response: Some("eu-west-1"),
                calls: 0,
            };

            // Target parsing owns ARN/endpoint syntax. This seam accepts the
            // Region it inferred from either canonical selector form.
            let resolved = resolve_region(
                RegionResolutionRequest::new(None, Some(selector_region), Some("eu-west-1"), true),
                &mut prompt,
            )
            .unwrap_or_else(|_| panic!("{selector_kind} Region resolves"));

            assert_eq!(resolved.region(), selector_region);
            assert_eq!(resolved.source(), RegionResolutionSource::Selector);
            assert_eq!(prompt.calls, 0);
        }
    }

    #[test]
    fn conflicting_explicit_and_selector_regions_are_usage_errors() {
        let mut prompt = FakePrompt {
            response: None,
            calls: 0,
        };

        let error = resolve_region(
            RegionResolutionRequest::new(
                Some("us-east-1"),
                Some("us-west-2"),
                Some("eu-west-1"),
                true,
            ),
            &mut prompt,
        )
        .expect_err("ambiguous Region must fail");

        assert_eq!(error.category(), ErrorCategory::Usage);
        assert_eq!(prompt.calls, 0);
    }

    #[test]
    fn fake_environment_and_shared_config_sdk_regions_resolve_without_prompting() {
        for (provider, sdk_region) in [
            ("environment", "ap-southeast-2"),
            ("shared config", "sa-east-1"),
        ] {
            let mut prompt = FakePrompt {
                response: Some("eu-west-1"),
                calls: 0,
            };

            let resolved = resolve_region(
                RegionResolutionRequest::new(None, None, Some(sdk_region), true),
                &mut prompt,
            )
            .unwrap_or_else(|_| panic!("{provider} SDK Region resolves"));

            assert_eq!(resolved.region(), sdk_region);
            assert_eq!(resolved.source(), RegionResolutionSource::SdkProvider);
            assert_eq!(prompt.calls, 0);
        }
    }

    #[test]
    fn interactive_prompt_is_only_used_after_all_other_sources_are_absent() {
        let mut prompt = FakePrompt {
            response: Some("ca-central-1"),
            calls: 0,
        };

        let resolved = resolve_region(
            RegionResolutionRequest::new(None, None, None, true),
            &mut prompt,
        )
        .expect("prompted Region resolves");

        assert_eq!(resolved.region(), "ca-central-1");
        assert_eq!(resolved.source(), RegionResolutionSource::InteractivePrompt);
        assert_eq!(prompt.calls, 1);
    }

    #[test]
    fn missing_noninteractive_region_is_a_credential_safe_usage_error() {
        let mut prompt = FakePrompt {
            response: Some("us-east-1"),
            calls: 0,
        };

        let error = resolve_region(
            RegionResolutionRequest::new(None, None, None, false),
            &mut prompt,
        )
        .expect_err("noninteractive mode cannot prompt");

        assert_eq!(error.category(), ErrorCategory::Usage);
        assert_eq!(prompt.calls, 0);
        assert!(!error.to_string().contains("us-east-1"));
        assert!(!error.to_string().contains("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn invalid_regions_from_every_source_are_credential_safe_usage_errors() {
        for (source, request, prompt_response) in [
            (
                "explicit flag",
                RegionResolutionRequest::new(Some("not-a-region"), None, None, true),
                Some("us-east-1"),
            ),
            (
                "SDK provider",
                RegionResolutionRequest::new(None, None, Some("not-a-region"), true),
                Some("us-east-1"),
            ),
            (
                "interactive prompt",
                RegionResolutionRequest::new(None, None, None, true),
                Some("not-a-region"),
            ),
        ] {
            let mut prompt = FakePrompt {
                response: prompt_response,
                calls: 0,
            };

            let error =
                resolve_region(request, &mut prompt).expect_err("invalid Region must be rejected");
            assert_eq!(error.category(), ErrorCategory::Usage, "{source}");
            assert!(!error.to_string().contains("AWS_SECRET_ACCESS_KEY"));
            assert!(!error.to_string().contains("not-a-region"));
        }
    }
}
