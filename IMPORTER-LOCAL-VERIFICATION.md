# Rust importer local verification — 2026-09-10

Branch: `feature/daily-squirt-local`; parent commit: `beba3fd`. Changes remain local and uncommitted. No push, CMS/database import, Cloudflare operation, real WordPress access, account migration or Claude invocation was performed. No tracked personal-data fixture was read, copied or staged; `git status --short -- data` is empty.

## Implemented

The former user/admin/profile-image and legacy migration-route orchestration is removed. The new executable uses manifest-authorized Daily Squirt run, lookup, upsert, media, transition and rollback APIs. Inputs use independent external exports, explicit exact source selections, strict current schema fields and source-key relationships. WordPress selection is authenticated when configured, fully paginated, checksum/modified/status checked, and uses original `date_gmt`. Default comments are excluded; the opt-in path only resolves existing approved identities and fetches all comment pages.

The HTTP client checks/pins every DNS answer, disables inherited proxies/automatic redirects, separates exact source/destination/local-object boundaries, limits time/bytes/attempts, honors Retry-After and suppresses upstream bodies in failures. Media bytes must match approved checksum/MIME/signature before multipart upload. HTML image/source-set/linked-original aliases use actual CMS ingress URLs, preserving figures/captions/galleries/CTA order; exact approved iframe sources are separate. No dry-run mappings or fabricated target IDs exist.

Checkpoints are external, locked and checksum-protected, written in 25-entry temporary/sync/rename/directory-sync batches. They bind manifest, exact selection, operator label and a target-origin hash. Resume rechecks native mappings per type and uses idempotent upserts. An uncertain success triggers lookup before retry. Import is draft-only, publication/comment release separate; rollback follows bounded native cursors and reports conflicts.

## Executed verification

- `cargo fmt --check`: passed.
- `cargo clippy --offline --locked --all-targets -- -D warnings`: passed.
- `cargo test --offline --locked`: **28 tests passed**, comprising 18 pure/source tests and 10 local HTTP protocol tests; no ignored tests.
- The initial sandboxed HTTP attempt failed to bind loopback. Automatic review then allowed the narrowly scoped offline test command; all final HTTP tests used ephemeral synthetic loopback servers only.
- The protocol suite includes actual 25- and 1,000-article client runs with source media fetch, multipart upload, ingress rewriting, exact records, repeat/resume, named failure/recovery and rollback requests. The receiving CMS is a protocol mock; this is not native acceptance evidence.
- Source checksum was compared with the actual current `headless-strapi/src/utils/ds-source.js` implementation. Golden SHA-256: `96ff7de5e123a1ac3a4408f597c1da6f3a7c0c5b9eb10b224313b16693a889db`, including NFC/CRLF normalization, numeric formatting, media and source-key relations.
- A fresh generated 1,000-article manifest passed the actual current CMS `approvedManifest` and `validateRunRequest` functions with `sourceAuthority:synthetic.local`; no Strapi process or database was started for that check.

## Native integration handoff

`README.md` documents every CLI flag, configuration, independent export envelopes, source/checksum rules and staged execution. `scripts/generate_synthetic_manifest.py` creates a new external 25/1,000-article bundle, supporting/operational records and a real generated PNG. It supplies no source-account data, comments or publication approval. Example generated artifact: `/private/tmp/ds-rust-generated-20260910-a/manifest.json` (source origin `http://localhost:19080`, hash `fb64a3665ce93bc751abc876752e73048d38a2b8017bed91fd097c858adf29f2`). Regenerate with the actual approved mock origin and a unique prefix for native execution.

Root must provide: current CMS + actual object store, designated mock source serving `synthetic.png`, a temporary scoped native custom API token in memory, trusted registration of the exact complete manifest, and canonical Website identity/isolated cleanup journal. Configure `localMediaBaseUrl` exactly (current native local value `http://localhost:19000/ds-local-public`). Use a literal loopback source/destination host when the configured address is IPv4; a hostname with mixed IPv4/IPv6 answers fails closed unless every answer equals the expected address.

Run supporting types first with a separate checkpoint/run, then exactly 25 selected articles, then the 1,000-article selection with a different run/checkpoint. The full selection exceeds 1,000 distinct IDs if supporting records are added, so keep the article run type-scoped. Re-uploading the approved media reuses/validates native storage and returns fresh ingress mapping. Repeat/resume/reconcile/rollback must then be observed against native PostgreSQL/R2, including after-image checks and other-brand row/object before/after invariants. Native variant/orphan/storage counts come from the actual media/store verification, not invented client counters. No exception is automatically waived; all unresolved records exit nonzero.

The CMS owner corrected GET reconciliation to inspect current record fingerprints and required accepted objects while this client was being built. That native change still requires the root's current-image run. This importer handoff is ready for that integration, but L09 is not accepted from the mock tests alone.

Source/test/lock bundle fingerprint: `bc6e1cf728bb5def4deed3f0b864ab18712444d1be1eaa2730ecaf7703ddc464` (SHA-256 of path/NUL/bytes/NUL entries, in this order: Cargo.toml, Cargo.lock, sorted src/**/*.rs, sorted tests/**/*.rs, scripts/generate_synthetic_manifest.py).
