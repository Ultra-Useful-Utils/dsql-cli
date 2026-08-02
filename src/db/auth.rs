#![allow(dead_code)] // Consumed by the Milestone 2 session connector.

use crate::{app::DatabaseRole, error::ApplicationError};
use aws_sdk_dsql::auth_token::{AuthTokenGenerator, Config};
use aws_types::SdkConfig;
use zeroize::Zeroizing;

/// An authentication token that is only available to database connection code.
pub(crate) struct DatabaseAuthenticationToken(Zeroizing<String>);

impl DatabaseAuthenticationToken {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Generates a fresh Aurora DSQL authentication token for a database role.
pub(crate) async fn generate_auth_token(
    sdk_config: &SdkConfig,
    endpoint_hostname: &str,
    role: &DatabaseRole,
) -> Result<DatabaseAuthenticationToken, ApplicationError> {
    let signer_config = Config::builder()
        .hostname(endpoint_hostname)
        .build()
        .map_err(|_| token_generation_failure(role))?;
    let generator = AuthTokenGenerator::new(signer_config);
    let token = match role {
        DatabaseRole::Admin => generator.db_connect_admin_auth_token(sdk_config).await,
        DatabaseRole::Custom(_) => generator.db_connect_auth_token(sdk_config).await,
    }
    .map_err(|_| token_generation_failure(role))?;

    Ok(DatabaseAuthenticationToken(Zeroizing::new(
        token.as_str().to_owned(),
    )))
}

fn token_generation_failure(role: &DatabaseRole) -> ApplicationError {
    let permission = match role {
        DatabaseRole::Admin => "dsql:DbConnectAdmin",
        DatabaseRole::Custom(_) => "dsql:DbConnect",
    };
    ApplicationError::runtime(format!(
        "could not generate Aurora DSQL authentication token; verify AWS credentials and {permission} permission"
    ))
}

#[cfg(test)]
mod tests {
    use super::generate_auth_token;
    use crate::app::DatabaseRole;
    use aws_credential_types::{
        Credentials,
        provider::{
            ProvideCredentials, SharedCredentialsProvider, error::CredentialsError, future,
        },
    };
    use aws_smithy_async::test_util::ManualTimeSource;
    use aws_types::{SdkConfig, region::Region};
    use std::time::{Duration, UNIX_EPOCH};

    fn sdk_config() -> SdkConfig {
        SdkConfig::builder()
            .credentials_provider(SharedCredentialsProvider::new(Credentials::new(
                "AKIDEXAMPLE",
                "test-secret",
                None,
                None,
                "test",
            )))
            .region(Region::new("us-east-1"))
            .time_source(ManualTimeSource::new(
                UNIX_EPOCH + Duration::from_secs(1_724_716_800),
            ))
            .build()
    }

    #[tokio::test]
    async fn admin_role_generates_an_admin_authentication_token() {
        let result = generate_auth_token(
            &sdk_config(),
            "example.dsql.us-east-1.on.aws",
            &DatabaseRole::Admin,
        )
        .await;
        let Ok(token) = result else {
            panic!("token generation succeeds");
        };

        assert!(token.as_str().contains("Action=DbConnectAdmin"));
    }

    #[tokio::test]
    async fn custom_role_generates_a_regular_authentication_token() {
        let result = generate_auth_token(
            &sdk_config(),
            "example.dsql.us-east-1.on.aws",
            &DatabaseRole::Custom("application".into()),
        )
        .await;
        let Ok(token) = result else {
            panic!("token generation succeeds");
        };

        assert!(token.as_str().contains("Action=DbConnect"));
        assert!(!token.as_str().contains("Action=DbConnectAdmin"));
    }

    #[tokio::test]
    async fn missing_credentials_returns_a_sanitized_diagnostic() {
        let config = SdkConfig::builder()
            .region(Region::new("us-east-1"))
            .time_source(ManualTimeSource::new(UNIX_EPOCH))
            .build();

        let result = generate_auth_token(
            &config,
            "example.dsql.us-east-1.on.aws",
            &DatabaseRole::Custom("application".into()),
        )
        .await;
        let Err(error) = result else {
            panic!("missing credentials fail");
        };

        assert_eq!(
            error.to_string(),
            "could not generate Aurora DSQL authentication token; verify AWS credentials and dsql:DbConnect permission"
        );
    }

    #[derive(Debug)]
    struct FailingCredentialsProvider;

    impl ProvideCredentials for FailingCredentialsProvider {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            future::ProvideCredentials::new(async {
                Err(CredentialsError::provider_error(std::io::Error::other(
                    "external failure containing signed-token-secret",
                )))
            })
        }
    }

    #[tokio::test]
    async fn failed_credentials_do_not_leak_the_provider_diagnostic() {
        let config = SdkConfig::builder()
            .credentials_provider(SharedCredentialsProvider::new(FailingCredentialsProvider))
            .region(Region::new("us-east-1"))
            .time_source(ManualTimeSource::new(UNIX_EPOCH))
            .build();

        let result = generate_auth_token(
            &config,
            "example.dsql.us-east-1.on.aws",
            &DatabaseRole::Admin,
        )
        .await;
        let Err(error) = result else {
            panic!("credential-provider failure propagates safely");
        };

        assert_eq!(
            error.to_string(),
            "could not generate Aurora DSQL authentication token; verify AWS credentials and dsql:DbConnectAdmin permission"
        );
        assert!(!error.to_string().contains("signed-token-secret"));
    }
}
