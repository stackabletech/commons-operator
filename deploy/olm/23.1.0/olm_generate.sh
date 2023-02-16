#!/usr/bin/env bash
# what should be the entry point for bundling/deploying - this script?
#   - split into a) dockers and yamls b) installation
#   - "make opm" (to update csv.yaml etc. and tee everything up)
# How often do we need to create a bundle - every time the manifests change? i.e. every release
#   - yes, every new version
# generate-manifests.sh has been removed from templating, can maybe be re-purposed for openshift stuff..
# manifests:
# - roles.yaml --> same as the ClusterRole definition in helm/zookeeper-operator/templates/roles.yaml
# - zookeeper*.crd.yaml --> same as helm/zookeeper-operator/crds/crds.yaml (less one annotation for helm.sh/resource-policy: keep)
# - csv.yaml --> specific to openshift
# - configmap.yaml --> embeds helm/zookeeper-operator/configs/properties.yaml
# is the templates folder used at all? is it in danger of being removed? can we use for openshift?
# how to parameterize the namespace?
# the operator installs even when the source namespace doesn't match in subscription.yaml
# the catalog-source deploys the operator w/o the subscription?
# Tasks:
#   - regenerate charts and then split crds.yaml instead of individual files
#   - 23.1.0 --> 23.4.0-rc0
#   - manually prepare a bundle for common and add it to the catalog and see what happens with the subscription
#   - does deleting the operator also clean up the crds? (no)
#   - split script into:
#         prepare operator bundle (operator code: make regenerate-opm)
#         one-off for catalog (stackable-utils)
#         packaging for all operators (stackable-utils, iterating over all operators)

set -euo pipefail
set -x

main() {
  VERSION="$1";

  if [ -d "bundle" ]; then
    rm -rf bundle
  fi

  opm alpha bundle generate --directory manifests \
  --package commons-operator-package --output-dir bundle \
  --channels stable --default stable

  docker build -t "docker.stackable.tech/sandbox/test/commons-operator-bundle:${VERSION}" -f bundle.Dockerfile .
  docker push "docker.stackable.tech/sandbox/test/commons-operator-bundle:${VERSION}"

  opm alpha bundle validate --tag "docker.stackable.tech/sandbox/test/commons-operator-bundle:${VERSION}" --image-builder docker
}

main "$@"
