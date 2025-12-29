# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Fixed

- BREAKING: Prevent Pod 0 restart by utilizing a mutating webhook.
  The commons-operator now needs the RBAC permission to `create` and `patch`
  `mutatingwebhookconfigurations`. The webhook can be disabled using
  `--disable-restarter-mutating-webhook` or by setting the `DISABLE_RESTARTER_MUTATING_WEBHOOK`
  env variable ([#387]).

[#387]: https://github.com/stackabletech/commons-operator/pull/387

## [25.11.0] - 2025-11-07

## [25.11.0-rc1] - 2025-11-06

### Added

- Helm: Allow Pod `priorityClassName` to be configured ([#376]).
- Add end-of-support checker ([#377]).
  - `EOS_CHECK_MODE` (`--eos-check-mode`) to set the EoS check mode. Currently, only "offline" is supported.
  - `EOS_INTERVAL` (`--eos-interval`) to set the interval in which the operator checks if it is EoS.
  - `EOS_DISABLED` (`--eos-disabled`) to disable the EoS checker completely.

### Changed

- Bump stackable-operator to `0.100.1` ([#381]).
- Reduce severity of Pod eviction errors. Previously, the operator would produce lot's of
  `Cannot evict pod as it would violate the pod's disruption budget` errors. With this fix, the
  error is reduced to an info instead ([#372]).
- Remove workaround for limiting rescheduling delay ([#378]).

[#372]: https://github.com/stackabletech/commons-operator/pull/372
[#376]: https://github.com/stackabletech/commons-operator/pull/376
[#377]: https://github.com/stackabletech/commons-operator/pull/377
[#378]: https://github.com/stackabletech/commons-operator/pull/378
[#381]: https://github.com/stackabletech/commons-operator/pull/381

## [25.7.0] - 2025-07-23

## [25.7.0-rc1] - 2025-07-18

### Added

- Adds new telemetry CLI arguments and environment variables ([#349]).
  - Use `--file-log-max-files` (or `FILE_LOG_MAX_FILES`) to limit the number of log files kept.
  - Use `--file-log-rotation-period` (or `FILE_LOG_ROTATION_PERIOD`) to configure the frequency of rotation.
  - Use `--console-log-format` (or `CONSOLE_LOG_FORMAT`) to set the format to `plain` (default) or `json`.
- Add RBAC rule to Helm template for automatic cluster domain detection ([#365]).

### Changed

- Replace stackable-operator `initialize_logging` with stackable-telemetry `Tracing` ([#338], [#344], [#349]).
  - BREAKING: The console log level was set by `COMMONS_OPERATOR_LOG`, and is now set by `CONSOLE_LOG_LEVEL`.
  - BREAKING: The file log level was set by `COMMONS_OPERATOR_LOG`, and is now set by `FILE_LOG_LEVEL`.
  - BREAKING: The file log directory was set by `COMMONS_OPERATOR_LOG_DIRECTORY`, and is now set
    by `FILE_LOG_DIRECTORY` (or via `--file-log-directory <DIRECTORY>`).
  - Replace stackable-operator `print_startup_string` with `tracing::info!` with fields.
- Version CRDs and bump dependencies ([#353]).
- Limit rescheduling delay to a maximum of 6 months ([#363]).
- BREAKING: Bump stackable-operator to 0.94.0 and update other dependencies ([#365]).
  - The default Kubernetes cluster domain name is now fetched from the kubelet API unless explicitly configured.
  - This requires operators to have the RBAC permission to get nodes/proxy in the apiGroup "". The helm-chart takes care of this.
  - The CLI argument `--kubernetes-node-name` or env variable `KUBERNETES_NODE_NAME` needs to be set. The helm-chart takes care of this.

### Fixed

- Use `json` file extension for log files ([#343]).
- Allow uppercase characters in domain names ([#365]).

### Removed

- Remove the `lastUpdateTime` field from the stacklet status ([#365]).
- Remove role binding to legacy service accounts ([#365]).

[#338]: https://github.com/stackabletech/commons-operator/pull/338
[#343]: https://github.com/stackabletech/commons-operator/pull/343
[#344]: https://github.com/stackabletech/commons-operator/pull/344
[#349]: https://github.com/stackabletech/commons-operator/pull/349
[#353]: https://github.com/stackabletech/commons-operator/pull/353
[#363]: https://github.com/stackabletech/commons-operator/pull/363
[#365]: https://github.com/stackabletech/commons-operator/pull/365

## [25.3.0] - 2025-03-21

### Removed

- BREAKING: Removed the deprecated pod enrichment controller ([#321]).

### Added

- Aggregate emitted Kubernetes events on the CustomResources ([#318]).
- Add the region field to the S3Connection CRD ([#331], [#335]).

### Changed

- Bump `stackable-operator` to 0.87.0 ([#334]).
- Default to OCI for image metadata ([#320]).

[#318]: https://github.com/stackabletech/commons-operator/pull/318
[#320]: https://github.com/stackabletech/commons-operator/pull/320
[#321]: https://github.com/stackabletech/commons-operator/pull/321
[#331]: https://github.com/stackabletech/commons-operator/pull/331
[#334]: https://github.com/stackabletech/commons-operator/pull/334
[#335]: https://github.com/stackabletech/commons-operator/pull/335

## [24.11.1] - 2025-01-09

## [24.11.0] - 2024-11-18

### Added

- The operator can now run on Kubernetes clusters using a non-default cluster domain.
  Use the env var `KUBERNETES_CLUSTER_DOMAIN` or the operator Helm chart property `kubernetesClusterDomain` to set a non-default cluster domain ([#290]).

### Changed

- BREAKING: Bump `stackable-operator` to 0.78.0 which includes a new `AuthenticationClassProvider` member for Kerberos. This will need to be considered when validating authentication providers ([#285]).

### Fixed

- BREAKING: The fields `connection` and `host` on `S3Connection` as well as `bucketName` on `S3Bucket`are now mandatory. Previously operators errored out in case these fields where missing ([#283]).
- Failing to parse one `ZookeeperCluster`/`ZookeeperZnode` should no longer cause the whole operator to stop functioning ([#293]).
- The StatefulSet restarter service now only retrieves metadata for ConfigMaps and Secrets, rather than full objects ([#293]).

[#283]: https://github.com/stackabletech/commons-operator/pull/283
[#285]: https://github.com/stackabletech/commons-operator/pull/285
[#290]: https://github.com/stackabletech/commons-operator/pull/290
[#293]: https://github.com/stackabletech/commons-operator/pull/293

## [24.7.0] - 2024-07-24

### Changed

- Bump `stackable-operator` to 0.70.0, and other dependencies ([#267]).

[#267]: https://github.com/stackabletech/commons-operator/pull/267

## [24.3.0] - 2024-03-20

### Added

- Helm: support labels in values.yaml ([#203]).

### Fixed

- Respect `--watch-namespace` CLI argument ([#193]).

[#193]: https://github.com/stackabletech/commons-operator/pull/193
[#203]: https://github.com/stackabletech/commons-operator/pull/203

## [23.11.0] - 2023-11-24

## [23.7.0] - 2023-07-14

### Added

- Generate OLM bundle for Release 23.4.0 ([#160]).

### Changed

- `operator-rs` `0.28.0` -> `0.44.0` ([#161], [#167]).

[#160]: https://github.com/stackabletech/commons-operator/pull/160
[#161]: https://github.com/stackabletech/commons-operator/pull/161
[#167]: https://github.com/stackabletech/commons-operator/pull/167

## [23.4.0] - 2023-04-17

### Added

- Generate OLM bundle ([#149])

[#149]: https://github.com/stackabletech/commons-operator/pull/149

### Changed

- Specified security context settings needed for OpenShift ([#136]).
- Revert openshift settings ([#142])
- Operator is now deployed by the Helm chart with resource limits ([#165])

[#136]: https://github.com/stackabletech/commons-operator/pull/136
[#142]: https://github.com/stackabletech/commons-operator/pull/142
[#165]: https://github.com/stackabletech/commons-operator/pull/165

## [23.1.0] - 2023-01-23

### Added

- Added `AuthenticationClass` provider static (bump operator-rs to `0.28.0`)  ([#123])

### Changed

- Bump operator-rs to `0.27.1` ([#116])

[#116]: https://github.com/stackabletech/commons-operator/pull/116
[#123]: https://github.com/stackabletech/commons-operator/pull/123

## [0.4.0] - 2022-11-07

## [0.3.0] - 2022-09-06

- Updates to library dependencies and templating scripts

## [0.2.1] - 2022-07-22

### Changed

- Include chart name when installing with a custom release name ([#57], [#58])

### Fixed

- Add permission to get kubernetes nodes to service-account ([#65])
- Added permission to create `pods/eviction` to ClusterRole for operator ([#67])

[#57]: https://github.com/stackabletech/commons-operator/pull/57
[#58]: https://github.com/stackabletech/commons-operator/pull/58
[#65]: https://github.com/stackabletech/commons-operator/pull/65
[#67]: https://github.com/stackabletech/commons-operator/pull/67

## [0.2.0] - 2022-06-30

### Added

- Pods are now annotated with their associated node's primary address ([#36])

### Changed

- `operator-rs` `0.18.0` -> `0.21.1` ([#38])

[#36]: https://github.com/stackabletech/commons-operator/pull/36
[#38]: https://github.com/stackabletech/commons-operator/pull/38

## [0.1.0] - 2022-05-04

### Changed

- Adapt to move of commons structs to operators-rs ([#18])

### Added

- Add restart controller ([#11])
- Add docs for AuthenticationClass and TLS ([#10])

[#10]: https://github.com/stackabletech/commons-operator/pull/10
[#11]: https://github.com/stackabletech/commons-operator/pull/11
[#18]: https://github.com/stackabletech/commons-operator/pull/18
