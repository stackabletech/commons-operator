# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

## [23.4.0] - 2023-04-17

### Added

- Generate OLM bundle ([#149])

[#149]: https://github.com/stackabletech/commons-operator/pull/149

### Changed

- Specified security context settings needed for OpenShift ([#136]).
- Revert openshift settings ([#142])

[#136]: https://github.com/stackabletech/commons-operator/pull/136
[#142]: https://github.com/stackabletech/commons-operator/pull/142

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
