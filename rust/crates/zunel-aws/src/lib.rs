//! AWS SSO credential auto-refresh helpers used by `zunel gateway`.
//!
//! Two surfaces:
//!
//! - [`sso_refresh`] — refresh one named profile by shelling out to
//!   `aws configure export-credentials`. Sized to mirror the
//!   [`zunel_channels::slack::bot_refresh`] precedent.
//! - [`profiles`] — discover the set of SSO-bearing profiles from
//!   `~/.aws/config` so the gateway can keep every logged-in role
//!   alive without forcing the user to re-enumerate them under
//!   `aws.ssoProfiles`.

pub mod profiles;
pub mod sso_refresh;

pub use profiles::{discover_sso_profiles, parse_sso_profiles, resolve_aws_config_path};
pub use sso_refresh::{
    refresh_profile_if_near_expiry, RefreshContext, RefreshError, RefreshOutcome, RefreshResult,
};
