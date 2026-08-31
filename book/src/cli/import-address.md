# The `import-address` command

`zallet import-address` imports a transparent address into a Zallet wallet as
watch-only. The wallet will track transactions involving the address, but will
not have spending authority. It covers the roles of both the `importaddress`
and (in part) `importpubkey` RPC methods of `zcashd`.

The command takes the address in one of three forms:

- the bare transparent address string,
- the hex-encoded public key, for a P2PKH address, or
- the hex-encoded redeem script, for a P2SH address.

A bare address import watches for the address's outputs but stores no key
material. An import by public key or redeem script tracks the same
transactions, and additionally allows the address to be upgraded to spending
capability if the corresponding private key material is imported later;
importing key material for a previously bare-imported address upgrades it in
place.

By default the address is imported into the account holding the legacy
`zcashd` pool of funds, as named by the `features.legacy_pool_seed_fingerprint`
option in the Zallet config file (whose value `zallet migrate-zcashd-wallet`
prints on import). Pass `--account <UUID>` to import into a different account;
account UUIDs can be obtained from a running Zallet wallet with
`zallet rpc z_listaccounts`.

By default the command connects to the configured chain backend and queues a
rescan from the account's birthday, so that the next `zallet start` discovers
the address's existing history. Pass `--no-rescan` to import without the chain
backend being available; only transactions in blocks scanned after the import
will then be detected.

The imported address is printed on success:

```
$ zallet import-address t1SRgDBrpcd4LNMUhGbBfh8S8rs7G335v6R
t1SRgDBrpcd4LNMUhGbBfh8S8rs7G335v6R
$ zallet import-address --account 514ab5f4-62bd-4d8c-94b5-23fa8d8d38c2 \
    03b0da749730dc9b4b1f4a14d6902877a92541f5368778853d9c4a0cb7802dcfb2
t1SRgDBrpcd4LNMUhGbBfh8S8rs7G335v6R
```

The public-key and redeem-script forms are also available on a running wallet
via the `z_importaddress` JSON-RPC method.
