use serde::{Deserialize, Serialize};

use crate::tls::Tls;
use crate::SecretClassVolume;
use stackable_operator::kube::CustomResource;
use stackable_operator::schemars::{self, JsonSchema};

#[derive(Clone, CustomResource, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[kube(
    group = "authentication.stackable.tech",
    version = "v1alpha1",
    kind = "AuthenticationClass",
    plural = "authenticationclasses",
    crates(
        kube_core = "stackable_operator::kube::core",
        k8s_openapi = "stackable_operator::k8s_openapi",
        schemars = "stackable_operator::schemars"
    )
)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticationClassSpec {
    /// Provider used for authentication like LDAP or Kerberos
    pub provider: AuthenticationClassProtocol,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthenticationClassProtocol {
    Ldap(LdapAuthenticationProvider),
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LdapAuthenticationProvider {
    /// Hostname of the LDAP server
    pub hostname: String,
    /// Port of the LDAP server. If TLS is used defaults to 636 otherwise to 389
    pub port: Option<u16>,
    /// LDAP search base
    #[serde(default)]
    pub search_base: String,
    /// LDAP query to filter users
    #[serde(default)]
    pub search_filter: String,
    /// The name of the LDAP object fields
    pub ldap_field_names: LdapFieldNames,
    /// In case you need a special account for searching the LDAP server you can specify it here
    pub bind_credentials: Option<SecretClassVolume>,
    /// Use a TLS connection. If not specified no TLS will be used
    pub tls: Option<Tls>,
}

impl LdapAuthenticationProvider {
    pub fn default_port(&self) -> u16 {
        match self.tls {
            None => 389,
            Some(_) => 636,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LdapFieldNames {
    /// The name of the username field
    #[serde(default = "LdapFieldNames::default_uid_field")]
    pub uid_field: String,
    /// The name of the group field
    #[serde(default = "LdapFieldNames::default_group_field")]
    pub group_field: String,
    /// The name of the firstname field
    #[serde(default = "LdapFieldNames::default_firstname_field")]
    pub firstname_field: String,
    /// The name of the lastname field
    #[serde(default = "LdapFieldNames::default_lastname_field")]
    pub lastname_field: String,
    /// The name of the email field
    #[serde(default = "LdapFieldNames::default_email_field")]
    pub email_field: String,
}

impl LdapFieldNames {
    fn default_uid_field() -> String {
        "uid".to_string()
    }

    fn default_group_field() -> String {
        "memberof".to_string()
    }

    fn default_firstname_field() -> String {
        "givenName".to_string()
    }

    fn default_lastname_field() -> String {
        "sn".to_string()
    }

    fn default_email_field() -> String {
        "mail".to_string()
    }
}
