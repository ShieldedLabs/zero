# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).


<!-- next-header -->

## [0.390.0] - unreleased, Zero-local

This version is not published; it is a local build maintained in the Zero repository.
See README.md.

### Changed
- The `rusqlite` requirement is `>=0.37, <0.40` rather than a single pinned minor.

### Fixed
- The adapter test suite ran zero tests, because a trailing comma in the
  `test_schemerz_adapter!` invocation selected the variadic arm with an empty
  test-name list. All five tests now run.

## [Unreleased]

## [0.370.0] - 2025-10-26
### Changed
- Migrated to `rusqlite 0.37`.

## [0.360.0] - 2025-10-26
### Changed
- Migrated to `rusqlite 0.36`.

## [0.350.0] - 2025-10-26
### Changed
- Migrated to `rusqlite 0.35`.

## [0.340.0] - 2025-10-26
### Changed
- Migrated to `rusqlite 0.34`.

## [0.330.0] - 2025-10-26
### Changed
- MSRV is now 1.82.
- Migrated to `rusqlite 0.33`.

## [0.320.0] - 2024-10-16
### Changed
- MSRV is now 1.77.
- Migrated to `rusqlite 0.32`.

## [0.310.0] - 2024-10-16
### Changed
- MSRV is now 1.63.
- Migrated to `rusqlite 0.31`.

## [0.300.0] - 2024-10-16
### Changed
- Migrated to `rusqlite 0.30`.

## [0.291.0] - 2024-10-16
### Changed
- Migrated to `schemerz 0.2.0`.
- **IMPORTANT BREAKING CHANGE**: `schemerz_rusqlite::RusqliteAdapter::new` now
  uses a default table name of `_schemerz` when the `table_name` argument is
  `None`. If you were not setting this argument before and are migrating from
  `schemer`, you will need to set `table_name` to `Some("_schemer".into())`.

## [0.290.0] - 2024-10-15
Initial release. The API is identical to `schemer-rusqlite 0.2.2`.


<!-- next-url -->
[Unreleased]: https://github.com/zcash/schemerz/compare/schemerz-rusqlite-0.370.0...HEAD
[0.370.0]: https://github.com/zcash/schemerz/compare/schemerz-rusqlite-0.360.0...schemerz-rusqlite-0.370.0
[0.360.0]: https://github.com/zcash/schemerz/compare/schemerz-rusqlite-0.350.0...schemerz-rusqlite-0.360.0
[0.350.0]: https://github.com/zcash/schemerz/compare/schemerz-rusqlite-0.340.0...schemerz-rusqlite-0.350.0
[0.340.0]: https://github.com/zcash/schemerz/compare/schemerz-rusqlite-0.330.0...schemerz-rusqlite-0.340.0
[0.330.0]: https://github.com/zcash/schemerz/compare/schemerz-rusqlite-0.320.0...schemerz-rusqlite-0.330.0
[0.320.0]: https://github.com/zcash/schemerz/compare/schemerz-rusqlite-0.310.0...schemerz-rusqlite-0.320.0
[0.310.0]: https://github.com/zcash/schemerz/compare/schemerz-rusqlite-0.300.0...schemerz-rusqlite-0.310.0
[0.300.0]: https://github.com/zcash/schemerz/compare/schemerz-rusqlite-0.291.0...schemerz-rusqlite-0.300.0
[0.291.0]: https://github.com/zcash/schemerz/compare/schemerz-rusqlite-0.290.0...schemerz-rusqlite-0.291.0
[0.290.0]: https://github.com/zcash/schemerz/compare/1bfd952b035b87a39df955376e0bdddf98eb6c99...schemerz-rusqlite-0.290.0
