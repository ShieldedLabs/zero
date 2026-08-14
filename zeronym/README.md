# Zeronym

Privacy-preserving Zcash light-wallet indexing. The documentation lives in **The Zeronym
Book**, an mdBook under [`book/`](./book), published at
**<https://shieldedlabs.github.io/zero/zeronym/>**.

The book covers the near-term turnstile-privacy system (the `zero-indexer-shim` +
`zero-indexer-hub` that protect **Orchard exits**, any transaction moving value out of
the Orchard pool, including but not limited to the Orchard to Ironwood migration, from
IP linkage) and the long-term vision (indexer + Nym + TEE + PIR).

Every push to `main` that touches `book/` rebuilds and republishes it
(`.github/workflows/zeronym-book.yml`).

## Read it locally

```
cargo install mdbook mdbook-mermaid   # one-time
mdbook-mermaid install book           # one-time (adds the diagram assets)
mdbook serve book --open              # serves at http://localhost:3000, live-reload
```

Or read the chapter sources directly under [`book/src/`](./book/src).
