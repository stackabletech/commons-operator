use stackable_commons_crd::authentication::AuthenticationClass;
use stackable_operator::crd::CustomResourceExt;

fn main() {
    built::write_built_file().expect("Failed to acquire build-time information");

    AuthenticationClass::write_yaml_schema("../../deploy/crd/authentication.crd.yaml").unwrap();
}
