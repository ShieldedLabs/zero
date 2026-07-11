# orchard [![Crates.io](https://img.shields.io/crates/v/orchard.svg)](https://crates.io/crates/orchard) #

Requires Rust 1.85.1+.

## Documentation

- [The Orchard Book](https://zcash.github.io/orchard/)
- [Crate documentation](https://docs.rs/orchard)

## `no_std` compatibility

In order to take advantage of `no_std` builds, downstream users of this crate
must enable the `spin_no_std` feature of the `lazy_static` crate. This is
needed because the `--no-default-features` build of `lazy_static` still relies
on `std`.

## Security Vulnerability Disclosure

For the Zero distribution of Orchard, maintained by Shielded Labs, report
security vulnerabilities through the Shielded Labs security disclosure group on
Signal:

https://signal.group/#CjQKICZtmwnx-qJlNzqu9ACZno_s9hMZhELfjod-KBGXVXxUEhA-p8Ai5BgwAVVllZvDV6tb

This group is a triage waiting room staffed by the Shielded Labs security team.
After you are admitted, say only that you have a report; do not post details
there. We will move you into a private group with the relevant people, where you
disclose the vulnerability, and then remove you from the waiting room.

## License

Copyright 2020-2023 The Electric Coin Company.

All code in this workspace is licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
