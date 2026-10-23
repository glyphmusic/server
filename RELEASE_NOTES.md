# Glyph CLI / Server 0.8.0 - security patch

- Fix the release publication race that produced mismatched SHA-256 checksums and blocked npm installation. Every archive and the npm tarball is verified together before publication.
- Signed update manifests, signed companion requests and a separate companion management token. Update the companion before deploying the Worker.
- Isolate client-supplied resolver hints from shared caches, restrict cover fetch hosts and validate provider identifiers.
- Support encrypted account sync and device pairing.

See CHANGELOG.md for the complete changes. npm publication remains a separate authenticated step.

- YouTube results get an explicit-content rating from Deezer where available; unknown ratings show a question mark.
