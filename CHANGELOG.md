# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Changed

- Bump `stackable-operator` to 0.70.0 ([#267]).

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
