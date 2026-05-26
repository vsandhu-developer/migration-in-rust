# WordPress -> Strapi Migration System (Rust)

A Rust port of the C++ migration system. Migrates blog articles, users, authors,
taxonomies, comments, and media from WordPress to Strapi v5.

## Build

```bash
cargo build --release
```

## Run

```bash
cargo run --release
```

Or build first:

```bash
cargo build --release
./target/release/migration-system
```

## Configuration

- `data/config/config.json` — base URLs and bearer token
- `data/config/pre-migration-config.json` — feature flags + batch sizes

```json
// data/config/config.json
{
  "wpBaseUrl": "https://example.com/",
  "strapiBaseUrl": "http://localhost:1337",
  "token": "<bearer token>"
}
```

Environment variable overrides: `WP_BASE_URL`, `STRAPI_BASE_URL`, `STRAPI_TOKEN`.

## Source data layout

```
data/
├── config/{config,pre-migration-config}.json
├── source/
│   ├── performers/DS-Performers.csv      (id,name,slug)
│   ├── studios/DS-studio.csv             (id,Studio,Slug)
│   ├── categories/DS-categories.json     (nested cat tree)
│   └── users/
│       ├── admin_users.json
│       ├── users.json
│       ├── author.json
│       └── image_variables.json
├── mappings/{pre-migration,users}/...    (outputs)
├── reports/                              (failure reports)
├── incomplete/                           (partial articles / failed comments)
└── processed/                            (idempotency tracker)
```

## Phases

1. **Pre-article migration** — performers, studios, categories, sub-categories
2. **User images** — download avatars + upload to Strapi
3. **Admin users** — bulk migrate admin accounts
4. **Users** — bulk migrate regular users (with reconciliation)
5. **Authors** — migrate ds-author content type entries
6. **Articles** — fetch posts, process content, upload images, post to Strapi
