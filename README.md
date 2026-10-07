# Daily Squirt importer

This executable uses only the current CMS `/api/ds-migration/*` contract. Content phases are manifest-authorized; the explicit `admin-users` and `users` phases restore the old account migration on the fixed `/api/ds-migration/{admin-users,users,users/lookup}` routes. It never calls the retired migration routes and never publishes during an import. Comments are excluded unless an approved external manifest maps them to existing users. The reference WordPress HTML/excerpt helpers are retained as pure source.

## Build and tests

```
cargo build --offline --locked
cargo fmt --check
cargo clippy --offline --locked --all-targets -- -D warnings
cargo test --offline --locked
```

`Cargo.lock` is tracked and explicitly unignored. Offline commands require the locked crates in the local Cargo cache. Protocol tests open ephemeral loopback mock servers and require local socket permission. They make no external calls. The 25/1,000 protocol tests check exact selection, checkpoint/resume/repeat/rollback requests and counts; they do **not** prove PostgreSQL, R2 storage or other-brand isolation. Those acceptance checks require the actual CMS and object store under the root local harness.

## Required inputs

Every content invocation supplies `--config`, `--manifest`, `--phase`, `--run-id`, `--source-ids` and `--types`; account phases supply `--config`, `--phase`, `--run-id`, `--users-file` and `--checkpoint` instead (see *Account phases*). There are no fallback URLs, credentials, default phases, first-N selections or automatic fixture paths. Run an inventory before an import. Invalid config/manifest/selection is rejected before network calls or checkpoint creation.

Configuration is strict JSON with these fields:

```json
{
  "target": "http://127.0.0.1:1337/",
  "targetTokenEnv": "DS_MIGRATION_API_TOKEN",
  "wordpress": "http://127.0.0.1:19080/wp-json/wp/v2/",
  "wordpressAuthorizationEnv": "DS_WORDPRESS_AUTHORIZATION",
  "allowPrivatePosts": false,
  "local": true,
  "localSourceOrigin": "http://127.0.0.1:19080/",
  "localTargetOrigin": "http://127.0.0.1:1337/",
  "localSourceAddress": "127.0.0.1",
  "localTargetAddress": "127.0.0.1",
  "localMediaBaseUrl": "http://localhost:19000/ds-local-public",
  "maxAttempts": 3,
  "timeoutSeconds": 20
}
```

`wordpress` and its authorization setting may be omitted for approved editorial exports. Credentials are read only from the named environment variables and retained in memory. The target variable contains a native scoped custom API token, without the `Bearer ` prefix. The WordPress variable contains the complete approved Authorization header. Set it securely in the parent process; do not put secrets in manifests, arguments or checked-in files. Private/draft/future/pending WordPress reads additionally require `allowPrivatePosts:true` and explicit approved status per selected record.

`local:true` works only with `sourceAuthority:synthetic.local` for the source exception. The exact configured origins include their ports. Every DNS answer must match the configured mock/container address; redirects undergo the same validation. Metadata/link-local/unspecified addresses are rejected even as local exceptions. For normal sources every address must be public, HTTPS and on the approved manifest origin list. DNS is checked and pinned separately on each hop/attempt, proxy inheritance is disabled, and authenticated redirects cannot change origin. The local media base is a separate exact public-object bucket prefix, never an additional fetch allowlist. It is permitted only for synthetic local runs.

## External approval bundle

The manifest and every referenced export must live outside this repository and `daily-squirt-code`; canonical paths reject symlink bypasses. No tracked CSV/JSON dataset is an approved input. The manifest contains:

- `schemaVersion:"v1"`, `sourceSystem:"wordpress"`, `sourceAuthority`, `sourceOwner`, `approvedAt`, `sourceLocation`, `repositoryFixture:false` and exact `sourceOrigins`.
- `types`, mapping the twelve allowed type names to exact string source IDs. Names are `category`, `subCategory`, `author`, `performer`, `studio`, `article`, `comment`, `header`, `footer`, `homepage`, `popup`, `adConfig`.
- `files[type]:{path,sha256}`, referencing independent external taxonomy, performer, studio, author/editorial and operational exports. `sha256` covers the exact export bytes. Each export repeats the ownership/authority/version/timestamp/location metadata and contains `records`. Operational content requires approved re-entry or an authoritative export; WordPress is not its source.
- `taxonomy.category[id].accessLevel` and `taxonomy.subCategory[id].{accessLevel,parentSourceKey}` with explicit lower-case `public` or `gated`. No Public fallback exists; a public child under a gated parent is rejected.
- `dependencies[type]:[sourceKey]` for approved existing records, where needed across foundation and article runs.
- Optional `media`, `comments` and `publication` approvals described below.
- `manifestHash`: SHA-256 of compact UTF-8 JSON with all object keys recursively sorted and only the top-level `manifestHash` omitted; arrays retain order. ECMAScript integer-key enumeration/number formatting applies. Register the exact complete object (including export hashes) in the trusted CMS `daily-squirt-migration.approvedManifests` configuration before starting. The importer cannot grant its own server approval.

Each record is `{sourceId,sourceUrl,data,wordpress?}`. `data` uses the current lowerCamel schema, with references shaped only as `{sourceKey}` or arrays of those. A source key is compact `JSON.stringify([sourceSystem,sourceId])`; keep the type namespace separate. For example, performer and studio ID `17` identify separate records. Never supply target document/numeric IDs, Website, source metadata, creator, publication or moderation fields in `data`. The CMS resolves those and enforces current canonical Website Access.

Author records have real approved names, without source email addresses or numeric defaults. Comments require manifest `comments:{mode:"existing-users"|"anonymized",approval,users:{sourceKey:existingNativeUserId}}`; anonymized mappings use exactly one explicitly approved existing identity. No implicit user migration exists. Imported comments remain pending. Anonymous or unmapped comments are failures unless the approved mapping explicitly resolves them.

For an article read from live WordPress, `wordpress` supplies `{status,modifiedGmt,primaryCategoryId,checksum}`. `wordpressCategoryMap[wordpressCategoryId]` in the manifest binds the selected primary category to the approved subCategory source key. The importer fetches **all pages of the exact include selection**, honors `--wp-per-page`, checks status/modified/checksum, and rejects missing/duplicate/unapproved IDs. The snapshot checksum is SHA-256 of canonical JSON containing `id,title,content,excerpt,date_gmt,modified_gmt,status,categories,author,slug,link`. Publication comes from `date_gmt`; `modified_gmt` remains source verification metadata. Author identity must match the approved author relation. A WordPress-backed comment additionally supplies `{postId,checksum}`; checksum covers the complete source comment JSON. Its content/date are refreshed from the fully paginated comments endpoint, never embedded replies.

## Media and HTML

`files.media` points at an independently hashed metadata/export envelope. Its records contain `{sourceId,url,aliases:[]}`. Each corresponding `media[id]` contains `{sourceKey,checksum,accessLevel:"public"|"explicit",mimeType,size,transformVersion:"v1",requiredFor:{type:[sourceIds]}}`. Every required asset has an explicit owning selection; optional assets use `required:false`. MIME types are PNG/JPEG/WebP/AVIF, maximum 20 MiB. The importer checks DNS/redirects, bounded response bytes, declared MIME, magic signature and original checksum before sending multipart bytes. The CMS independently decodes and verifies pixels, creates variants, owns classification and journals accepted storage.

The multipart parts are `manifest` JSON plus `file`. The server returns actual target identity and canonical `ingressUrl`. Public ingress must be the canonical Daily Squirt public-object path; protected ingress is an unsigned `/api/ds-media/asset-<hash>/original` reference. These never contain bearer/signature/private-storage origins. Native server sanitization decides eventual guest/reader output.

An optional approved `frameSources` array contains exact HTTPS iframe URLs, matching the CMS configured frame list. Those frames are preserved separately from stored raster media; unapproved frames fail import.

Old-importer parity at import: the article slug uses the old `sanitize_slug` normalisation; top-level WordPress CTA buttons (old `ContentParser` rules) are removed from the body and sent as `registrationCtas` `{enabled:true, afterParagraph: old block index (min 1), label, url}`. `<video>`/`<source>` `src` passes through unchanged (HTTPS only; the CMS enforces its video-origin allowlist); posters are images. Request-scoped WordPress render artifacts (gallery ordinals, `[video]` IE9 shim, player instance ids, `?_=N` cache-busters) are stabilised before checksum and import.

HTML DOM rewriting replaces approved cover/inline/gallery/srcset/linked-original aliases with returned ingress URLs, retaining captions, element order and CTA markup. Missing images remain named failures, never successful hotlinks. Original and resized source aliases may deliberately refer to one approved original, with native variant generation handled by the CMS. If an independent original/variant has different bytes, give it its own media source identity/checksum. A changed checksum cannot overwrite an accepted mapping silently.

## Commands and phases

| Flag | Meaning |
|---|---|
| `--config PATH` | Required strict local configuration |
| `--manifest PATH` | Required approved external bundle |
| `--phase PHASE` | `inventory`, `import`, `reconcile`, `publish`, `unpublish`, `release-comments`, `rollback`, `admin-users`, `users` |
| `--run-id LABEL` | Required safe operator run label; CMS run ID is created deterministically and persisted separately |
| `--source-ids ID,ID` | Exact selected source IDs, at most 1,000 distinct IDs |
| `--types TYPE,TYPE` | Exact approved type namespaces |
| `--checkpoint PATH` | Required for writes; existing external parent directory, new run directory |
| `--dry-run` | Inventory/import validation only; no target writes, checkpoint directory or mapping mutation |
| `--skip-images` | Inventory/dry-run only; no claim of verified/accepted media |
| `--resume` | Required for existing checkpoints and reconcile/transition/rollback phases |
| `--wp-per-page 50` | WordPress page size 1–100 for all post/comment selection modes |
| `--media-concurrency N` | Content phases: parallel source-fetch + CMS media uploads, 1–8 (default 2); record writes stay sequential |
| `--users-file PATH` | Account phases only: JSON array of `{wpId,username,email,slug,image,roles}` |
| `--batch-size N` | Account phases only: `users` 1–200 (default 200), `admin-users` 1–100 (default 100) |

Example (synthetic paths/labels only):

```
cargo run --offline --locked -- --config /private/tmp/ds-config.json --manifest /private/tmp/ds-approved/manifest.json --phase inventory --run-id qa-foundation --source-ids qa-author-1 --types author
cargo run --offline --locked -- --config /private/tmp/ds-config.json --manifest /private/tmp/ds-approved/manifest.json --phase import --run-id qa-foundation --source-ids qa-author-1 --types author --checkpoint /private/tmp/ds-foundation
```

Repeat the import with the same arguments plus `--resume`. Use the same immutable selection/checkpoint with `--phase reconcile` or `--phase rollback --resume`. A changed manifest/selection needs a new approved run. The importer never substitutes its own local label for the CMS-issued run key.

Use foundation runs for taxonomy/authors/performers/studios before article runs, then operational records referencing articles. The 1,000-source-ID limit includes all selected types: do not add supporting IDs to an already 1,000-article selection. Existing supporting identities can be declared in the same manifest and reused by the article run. Required-media reconciliation follows the run selection. Publish supporting records before articles; use separate type-scoped runs when an approval exceeds the 100-transition batch limit. Publishing and unpublishing require explicit manifest action/type/source-key lists; comment release is separately approved.

Checkpoint files are locked against concurrent writers, use checksum-protected bounded 25-record batches, write temporary files followed by file sync/rename/directory sync, and exclude content, credentials and source/ingress URLs. Resume verifies the server's current logical mappings once per type and performs checksum/fingerprint-protected idempotent upserts. Interrupted partial batches are recovered from server state. A lost write response triggers a source-key lookup before retry. HTTP 429 obeys Retry-After; retryable transport/5xx failures have bounded attempts, permanent 4xx failures do not loop. Requests with an out-of-budget Retry-After stop without retrying early.

JSON stdout reports exact selected/created/updated/skipped/failed/media counts and CMS run ID. Named failures contain type, a source-ID hash, safe code and HTTP status; no emails, URLs, response bodies, credentials or source text. Any failure, missing reconciliation entry or rollback conflict exits nonzero. No exception is waived automatically: failed records must be corrected or removed through a separately owned/reasoned new approved selection. For completed imports `selected = created + updated + skipped + exceptions + failed`; exceptions remain zero unless a future explicitly approved workflow provides them.

## Account phases (`admin-users`, `users`)

```
cargo run --offline --locked -- --config /private/tmp/ds-config.json --phase admin-users --run-id accounts-admin --users-file /secure/admin_users.json --checkpoint /private/tmp/ds-admin-users
cargo run --offline --locked -- --config /private/tmp/ds-config.json --phase users --run-id accounts-users --users-file /secure/users.json --batch-size 200 --checkpoint /private/tmp/ds-users
```

Run admin users first, then users. Target and token come from `target`/`targetTokenEnv`. `--dry-run` validates the file and prints counts without token, network or checkpoint. Records POST in batches to `/api/ds-migration/admin-users` (≤100) or `/api/ds-migration/users` (≤200) as `{data:[{wpId,username,email,slug,roles}]}`; `image` is accepted and ignored (user image upload was disabled in the old importer too). A missing email is sent as `migrated+{wpId}@example.invalid` and counted as `placeholderEmail`. HTTP retry/429/timeout behaviour is the shared client's. After the batches, failed regular users are looked up via `/api/ds-migration/users/lookup` by the email actually sent and mapped if found. Remaining `username_taken` failures then try the original login name; only responses explicitly marked `foundBy: "username"` are accepted. Account attributes are not overwritten. Admin users have no lookup.

The checkpoint directory (external parent, 0700, locked) holds `users-state.json` (0600, atomic, checksum-protected): per-wpId `status`/`code` plus `mapping` `{wpId: userId}` usable for an approved `comments.users` mapping. It binds the phase, run label, users-file SHA-256 and target. Re-run with `--resume` to retry only incomplete wpIds; completed wpIds are counted as `alreadyComplete`. Stdout is `{phase,selected,created,existing,failed,placeholderEmail,alreadyComplete,recovered,batches,failures:[{wpId,code,status?}]}` — never emails or usernames. Any failure exits nonzero.

## WordPress manifest generation

```
python3 scripts/generate_wordpress_manifest.py --count 1000 --batch-size 100 --out-dir /private/tmp/ds-wp-manifests --cache-dir /private/tmp/ds-wp-cache
```

Reads the public REST API (sequential, ≤2 req/s; image bytes for checksums via a bounded `--download-workers` pool, default 4, max 8; everything cached) plus the category/performer/studio/author exports, and writes `foundation/`, `articles-NN/` (newest published posts with featured + inline media, per-post `wordpress` snapshot checksum, `wordpressCategoryMap`, dependencies) and `SUMMARY.json` (counts, media bytes/classification, skipped posts with reasons). Media is `explicit` when the article's subCategory is gated, otherwise `public`. Posts with unmapped categories/authors, no featured image, unfetchable/non-raster media, script/audio/embed sources or gated subCategories (no approved public cover) are skipped and listed. Video sources are not rehosted; `SUMMARY.json.videoOrigins` lists the HTTPS origins to put in the CMS `DAILY_SQUIRT_VIDEO_SOURCES_JSON` allowlist. Approved comments are fetched from the paginated `/comments?post=ID` endpoint; they enter the article manifest (`types.comment`, `comments.mode:"existing-users"`) only with `--comments-approval`, authors mapped through `--user-mapping` (the `users-state.json` of reader-user phases only; admin-users IDs belong to a separate table and are rejected) and optionally an explicit `--fallback-user-id` (the old importer used 3; both the generator and orchestrator have no default; use a dedicated blocked reader identity for anonymous comments). Comments longer than the CMS `ds-comment.comment` limit for migrated comments (5,000 UTF-16 units since headless-strapi `4b061c2`; reader-typed comments stay 350) would fail the run, so they are left out and listed in `SUMMARY.json.comments.skippedOverLengthIds`. Import comments as a separate `--types comment` run (≤1,000 IDs of `comment-source-ids.txt` per run); release stays a separate `release-comments` step. Validate with `--phase inventory --dry-run` (articles re-fetch the posts from WordPress) and register each `manifest.json` verbatim before importing.

**Foundation scope.** `--foundation-scope all` (default) matches the old Pre/Authors phases: **every** category and subCategory of `DS-categories.json`, every author of `author.json` and every performer/studio row of `DS-Performers.csv`/`DS-studio.csv` (currently 2 / 4 / 13 / 1,838 / 62). `--foundation-scope referenced` keeps the earlier behaviour (only records the selected posts use, plus parent categories). The CSVs are read positionally like the old `load_performers`/`load_studios`: header skipped, column 0 id (must be > 0), column 1 trimmed name (required), column 2 trimmed slug (may be empty); other rows are skipped and counted. Because CMS uid fields accept only `^[a-z0-9]+(?:-[a-z0-9]+)*$` (≤128), slugs are lower-cased/hyphenated (three performer slugs contain `_`), an empty slug is derived from the name and a collision gets `-<id>`; every adjustment is counted in `SUMMARY.json.foundation.sourceFiles`. A missing category `accessLevel` defaults to Public as before.

The foundation is **one** manifest (registered once) whose selection exceeds the 1,000-ID run limit, so `foundation/selection.json` lists ordered per-type chunks of ≤1,000 IDs — `category`, `subCategory`, `author`, `performer` (2 parts), `studio` — each imported as its own run/checkpoint (`--types <type> --source-ids <chunk>`) and later published with the same run/checkpoint.

### Comments for existing articles

```
python3 scripts/generate_wordpress_manifest.py --post-ids 101,102 --out-dir /secure/ds-comments-1 --cache-dir /secure/ds-wp-cache --comments-approval APPROVAL-REF --publication-approval APPROVAL-REF --user-mapping /secure/ds-users/users-state.json --user-mapping /secure/ds-admin-users/users-state.json --fallback-user-id 3
```

For posts whose articles were already imported by an earlier run (same source key `["wordpress","<postId>"]`), `--post-ids` or `--post-ids-file` (commas/whitespace) replaces `--count` and writes comments-only `comments-NN/` manifests (`--batch-size` posts each, default 100): `types.comment` + `files.comment`, `comments:{mode:"existing-users",approval,users}`, `dependencies.article` for the posts that have comments and, with `--publication-approval`, `publication:{approval,releaseComments}` only. Posts are never fetched or filtered in this mode, so articles in gated/explicit subCategories keep their comments. Every comment keeps `wordpress:{postId,checksum}` and is refreshed from WordPress at import. Both the importer and the CMS resolve the `article` relation through `dependencies` (`relationAllowed`); the article does not have to be in the manifest's `types`, but it must exist on the target, otherwise the CMS fails the record with `migration_relation_missing`. Register each manifest, then import, reconcile and release:

```
cargo run --offline --locked -- --config /secure/ds-config.json --manifest /secure/ds-comments-1/comments-01/manifest.json --phase import --run-id comments-existing-01 --source-ids <comment-source-ids.txt> --types comment --checkpoint /secure/ds-cp/comments-existing-01
cargo run --offline --locked -- --config /secure/ds-config.json --manifest /secure/ds-comments-1/comments-01/manifest.json --phase release-comments --resume --run-id comments-existing-01 --source-ids <same> --types comment --checkpoint /secure/ds-cp/comments-existing-01
```

## Full migration orchestrator

`scripts/run_full_migration.py` (Python 3 standard library) reproduces the old single command (`beba3fd`: Pre → AdminUsers → Users → Authors → Articles + comments) with the new pieces. User images stay disabled, as in the old `pre-migration-config.json`. Build the importer first (`cargo build --release --offline --locked`; `--binary` overrides) and export the scoped CMS token in the variable named by the config's `targetTokenEnv`.

```
export DS_MIGRATION_API_TOKEN=...            # never in arguments or files
python3 scripts/run_full_migration.py --state-dir /secure/ds-full-run --config /secure/ds-config.json \
  --count 19000 --batch-size 100 --comments-approval APPROVAL-REF --publication-approval APPROVAL-REF
# ... commit the listed manifests to headless-strapi/config/ds-migration-manifests via reviewed PR, deploy ...
python3 scripts/run_full_migration.py --state-dir /secure/ds-full-run --continue \
  --registered-manifest-dir ../../headless-strapi/config/ds-migration-manifests
```

| Stage | Action |
|---|---|
| `admin-users`, `users` | Account phases (`data/source/users/admin_users.json`, `users.json` by default), checkpoints `<state>/checkpoints/{admin-users,users}` |
| `generate` | Generator into `<state>/manifests` (atomic rename from `manifests.partial`), `--user-mapping` users then admin-users state, `--fallback-user-id 3` unless `--fallback-user-id N`/`--no-fallback-user`, cache `<state>/wp-cache` |
| `register` | **Stop.** Prints every `manifest.json` path, hash and suggested file name. The CMS accepts only registered manifests; the orchestrator never registers them. Passed only by a re-run with `--continue` (optionally verified read-only against a local `--registered-manifest-dir`: same hash and identical JSON) |
| `inventory` | `--phase inventory --dry-run` for every foundation chunk, article batch and comment chunk (`--inventory-skip-images` to skip media fetches) |
| `foundation` | Import per type per ≤1,000 chunk of `foundation/selection.json` |
| `articles` | Import of each `articles-NN` (`--types article`) |
| `comments` | Import of each `articles-NN` comment chunk (`--types comment`, ≤1,000 IDs) |
| `publish-foundation`, `publish-articles` | `--phase publish --resume` with each unit's import run/checkpoint |
| `release-comments` | `--phase release-comments --resume` per comment chunk |

Every unit has its own run label (`<run-prefix>-fdn-performer-2`, `-art-01`, `-cmt-01-1`, …) and checkpoint `<state>/checkpoints/<unit>`; an import resumes (`--resume`) whenever its checkpoint exists. `<state>/orchestrator-state.json` (0600) records each completed unit/stage with the importer's count report, so a re-run of the same command skips completed work and resumes the failed unit; logs go to `<state>/logs/` (0600). The first real run fixes `--config`, `--count`, `--batch-size`, approvals, fallback, scope, run prefix, users files and `--wp-api` for the state directory; later runs may omit them, and a different value is rejected. `--only users,admin-users` runs just those stages, `--from STAGE` that stage onward (post-gate stages require the gate to have been passed). `--dry-run` prints the exact commands of every pending stage and writes nothing. Any unit failure stops with its safe error code; `--allow-account-failures` restores the old continue-on-failure behaviour for accounts (their comments then use the fallback user). The state directory must be outside the repository. Token, emails and usernames are never printed; the token only reaches the importer through the environment.

## Python tests

```
python3 -m unittest discover -s scripts/tests
```

Offline: a WordPress double (`scripts/tests/fake_wordpress.py`, synthetic posts/comments/authors/mappings plus the real taxonomy/performer/studio files) drives the generator; a fake importer/generator drives the orchestrator (gate, resume, idempotence, ordering, ≤1,000-ID chunks, settings lock, token check). `cargo test` additionally loads the generated full foundation and a comments-only manifest through the importer's manifest/selection/record/preflight validation.

## New synthetic input generation

```
python3 scripts/generate_synthetic_manifest.py --output /private/tmp/ds-generated --source-origin http://127.0.0.1:19080 --count 1000 --prefix qaunique
```

This creates new category/subCategory/author/performer/studio/article/header/footer/homepage/popup/ad exports, a generated PNG, and a manifest. It reads no repository fixtures, calls no network and does not approve the manifest on the CMS. Comments and publication are intentionally absent. Serve the generated PNG only through the designated local mock host. Use exactly 25 explicitly selected article IDs for canary verification, then exactly 1,000; record actual CMS, database, object-store reconciliation, repeat/resume and rollback evidence under the root local harness before accepting the migration. The generator itself and mock-protocol tests are not that evidence.

Comment author mappings can be supplied to the coordinator with repeatable `--comment-user-mapping` arguments. For WordPress editors, create reader profiles through the `users` phase and supply that checkpoint alongside the regular-reader checkpoint. Never use the `admin-users` checkpoint for comments. The chosen mapping paths and fallback reader ID are pinned in the coordinator state for resumed runs.
