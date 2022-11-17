# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Changed

- Bump operator-rs to `0.27.0` ([#116])

[#116]: https://github.com/stackabletech/commons-operator/pull/116

## [0.4.0] - 2022-11-07

## [0.3.0] - 2022-09-06

- Updates to library dependencies and templating scripts

## [0.2.1] - 2022-07-22

### Changed

- Include chart name when installing with a custom release name ([#57], [#58]).

### Fixed

- Add permission to get kubernetes nodes to service-account ([#65])
- Added permission to create `pods/eviction` to ClusterRole for operator ([#67])

[#57]: https://github.com/stackabletech/commons-operator/pull/57
[#58]: https://github.com/stackabletech/commons-operator/pull/58
[#65]: https://github.com/stackabletech/commons-operator/pull/65
[#67]: https://github.com/stackabletech/commons-operator/pull/67

## [0.2.0] - 2022-06-30

### Added

- Pods are now annotated with their associated node's primary address ([#36]).

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
