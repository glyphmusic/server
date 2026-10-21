# Releasing Glyph CLI / Server

The Rust server and npm package share a version. No npm, GitHub or Cloudflare
credentials belong in the source tree or npm payload.

1. Update `Cargo.toml`, `npm/package.json`, `npm/package-lock.json`, `CHANGELOG.md`
   and `RELEASE_NOTES.md`. If shared wire types changed, run
   `node scripts/snapshot-protocol.mjs` with the desktop checkout alongside this
   repository. The snapshot is already included for ordinary source builds.
   Always run `node scripts/snapshot-changelogs.mjs` (desktop, companion and r-dlp
   checkouts alongside) so glyph.my/changelog lists the release, and decide whether
   the change needs a page in the docs repository (docs.glyph.my); write one for
   any new user-facing feature or setting.
2. Run Rust tests, the Worker WASM build/integration tests, and `npm test` inside
   `npm/`. For the real native installer test, set `GLYPH_TEST_BINARY` to a binary
   built from this version before running npm tests.
3. After reviewing/committing the intended changes, push its matching `vX.Y.Z`
   tag. The Release workflow builds five native targets and the Worker, packs
   the npm payload, and only makes the GitHub release public after all artifacts
   and `SHA256SUMS.txt` are available. Installers require that matching release.
4. Publish the prepared tarball after `npm login`, or use the separate
   **Publish npm CLI** workflow with the release tag and an appropriately scoped
   repository `NPM_TOKEN` secret. The workflow verifies the tarball checksum
   before publishing it. npm may require account-specific 2FA/OTP approval.

```sh
gh release download v0.5.0 --repo glyphmusic/server --pattern glyph-server-0.5.0.tgz --pattern SHA256SUMS.txt
# Verify the exact tarball checksum against SHA256SUMS.txt first.
npm login
npm publish ./glyph-server-0.5.0.tgz --access public
```

Verify `npm view glyph-server version`, then install into a clean prefix and run
`glyph --version`, `glyph help`, `glyph setup cloudflare --help`. A local pack or
publish dry-run is not proof of a public npm release. Actual Cloudflare deploys,
remote updates and Docker recreation require their own environment validation.

After every release, redeploy the Worker (`wrangler deploy` in
`crates/glyph-server-worker`) and check that glyph.my/changelog shows the new
version: the tag does not deploy the site.
