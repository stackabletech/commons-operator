use serde::{Deserialize, Serialize};
use stackable_operator::builder::SecretOperatorVolumeSourceBuilder;
use stackable_operator::k8s_openapi::api::core::v1::CSIVolumeSource;

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
    #[serde(default = "default_uid_field")]
    pub uid_field: String,
    /// The name of the group field
    #[serde(default = "default_group_field")]
    pub group_field: String,
    /// The name of the firstname field
    #[serde(default = "default_firstname_field")]
    pub firstname_field: String,
    /// The name of the lastname field
    #[serde(default = "default_lastname_field")]
    pub lastname_field: String,
    /// The name of the email field
    #[serde(default = "default_email_field")]
    pub email_field: String,
}

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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretClassVolume {
    /// [SecretClass](https://docs.stackable.tech/secret-operator/secretclass.html) containing the LDAP bind credentials
    pub secret_class: String,
    /// [Scope](https://docs.stackable.tech/secret-operator/scope.html) of the [SecretClass](https://docs.stackable.tech/secret-operator/secretclass.html)
    pub scope: Option<SecretClassVolumeScope>,
}

impl SecretClassVolume {
    pub fn to_csi_volume(&self) -> CSIVolumeSource {
        let mut secret_operator_volume_builder =
            SecretOperatorVolumeSourceBuilder::new(&self.secret_class);

        if let Some(scope) = &self.scope {
            if scope.pod {
                secret_operator_volume_builder.with_pod_scope();
            }
            if scope.node {
                secret_operator_volume_builder.with_node_scope();
            }
            for service in &scope.services {
                secret_operator_volume_builder.with_service_scope(service);
            }
        }

        secret_operator_volume_builder.build()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretClassVolumeScope {
    #[serde(default)]
    pub pod: bool,
    #[serde(default)]
    pub node: bool,
    #[serde(default)]
    pub services: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tls {
    /// The verification method used to verify the certificates of the server and/or the client
    verification: TlsVerification,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TlsVerification {
    /// Use TLS but don't verify certificates
    None {},
    /// Use TLS and ca certificate to verify the server
    ServerVerification(TlsServerVerification),
    /// Use TLS and ca certificate to verify the server and the client
    MutualVerification(TlsMutualVerification),
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsServerVerification {
    /// Ca cert to verify the server
    pub server_ca_cert: CaCert,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsMutualVerification {
    /// [SecretClass](https://docs.stackable.tech/secret-operator/secretclass.html) which will provide ca.crt, tls.crt and tls.key
    pub cert_secret_class: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CaCert {
    /// Use TLS and the ca certificates trusted by the common web browsers to verify the server.
    /// This can be useful when you e.g. use public AWS S3 or other public available services.
    WebPki {},
    /// Name of the SecretClass which will provide the ca cert.
    /// Note that a SecretClass does not need to have a key but can also work with just a ca cert.
    /// So if you got provided with a ca cert but don't have access to the key you can still use this method.
    SecretClass(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn test_secret_class_volume_to_csi_volume() {
        let secret_class_volume = SecretClassVolume {
            secret_class: "myclass".to_string(), // pragma: allowlist secret
            scope: Some(SecretClassVolumeScope {
                pod: true,
                node: false,
                services: vec!["myservice".to_string()],
            }),
        }
        .to_csi_volume();

        let expected_volume_attributes = BTreeMap::from([
            (
                "secrets.stackable.tech/class".to_string(),
                "myclass".to_string(),
            ),
            (
                "secrets.stackable.tech/scope".to_string(),
                "pod,service=myservice".to_string(),
            ),
        ]);

        assert_eq!(
            expected_volume_attributes,
            secret_class_volume.volume_attributes.unwrap()
        );
    }
}
