# apphost

Self-hosted replacement for Claude Artifacts. Claude publishes HTML pages through an MCP server.
apphost stores them in your S3 bucket and serves them privately behind Microsoft Entra SSO. Pages
can keep **shared, live state** (shared todo lists, sign-up sheets, trackers, polls) through a
built-in SDK.

One Rust binary provides:

| Piece | What it does |
|---|---|
| **Portal** (`https://apphost.corp.example`) | Entra sign-in, artifact list and detail pages, sharing, versions, data browser, token management |
| **Content origins** (`https://<id>.apphost-usercontent.corp.example`) | Serves each artifact from its own origin, behind SSO and a per-artifact access check |
| **Shared-state API + SDK** | JSON documents in collections, per artifact, with live updates over server-sent events |
| **MCP server** (`/mcp`) | Tools for Claude: publish, list, read, share, delete artifacts; read and write their data |
| **OAuth 2.1 server** | Lets Claude connectors sign in with Entra (dynamic client registration + PKCE). Personal access tokens are also supported |

```
                    ┌──────────── apphost (ECS Fargate, N tasks) ────────────┐
Claude ──MCP/OAuth──►  /mcp, /oauth/*        portal UI + /api/*             │
                    │        │                     │                        │
Browser ──SSO──────►  <id>.usercontent  ──►  files (S3)   data (Postgres)    │
                    │        ▲  SSE live updates ◄── LISTEN/NOTIFY ──┘        │
                    └───────────────────────────────────────────────────────┘
                       Entra ID (OIDC)        S3 bucket        RDS Postgres
```

## Quick start (local)

Requirements: Docker. Rust 1.85+ only if you run it outside Docker.

```bash
docker compose up --build
```

Open http://localhost:8080 and sign in with any email. Development login has no password and is
only enabled by `APPHOST_DEV_LOGIN=true`. Artifacts open at `http://<id>.localhost:8080`, since
browsers resolve `*.localhost` to 127.0.0.1.

If port 8080 is taken, set `APPHOST_PORT=8090` and change `APPHOST_PUBLIC_URL` and
`APPHOST_CONTENT_DOMAIN` in `docker-compose.yml` to match.

Running natively against the compose Postgres, with files on local disk:

```bash
docker compose up -d postgres
DATABASE_URL=postgres://apphost:apphost@localhost:5432/apphost APPHOST_DEV_LOGIN=true \
APPHOST_STORAGE=fs APPHOST_SECRET=local-development-secret-change-me-0123456789 cargo run
```

Migrations run automatically at startup.

## Connecting Claude

The portal's **Connect Claude** page shows the MCP URL and these steps.

- **Claude (web/desktop), OAuth:** Settings → Connectors → Add custom connector → paste
  `https://apphost.corp.example/mcp` → Connect → sign in with Entra → Allow.
- **Claude Code, OAuth:** run `claude mcp add --transport http apphost https://apphost.corp.example/mcp`,
  then `/mcp` → apphost → Authenticate.
- **Personal access token:** create one on the Connect page, then use it with
  `claude mcp add --transport http apphost <url> --header "Authorization: Bearer aph_pat_…"`.
  Tokens last 1–365 days and can be revoked any time.

Your Claude organization admin must allow custom connectors.

> **Network reachability:** connectors added on claude.ai (web and desktop) are called from
> Anthropic's cloud. For those, `/mcp`, `/oauth/token`, `/oauth/register` and `/.well-known/*` must
> be reachable from the internet. You can restrict that to Anthropic's published egress IP ranges.
> The portal, `/oauth/authorize` and artifact pages are only used by employees' browsers, so they
> can stay internal or VPN-only. Claude Code connects from the user's own machine, so an
> internal-only deployment works for Claude Code.

### MCP tools

| Tool | Purpose |
|---|---|
| `get_authoring_guide` | Hosting rules, CSP, and the shared-state SDK reference with an example |
| `publish_artifact` | Create an artifact, or publish a new version in place. Files you omit are kept |
| `list_artifacts`, `get_artifact`, `read_artifact_file` | Browse artifacts and read their source |
| `share_artifact` | Set visibility (private/org) and add or remove people or Entra groups as viewer/editor |
| `delete_artifact` | Delete permanently (owner only) |
| `data_list`, `data_write`, `data_delete` | Read, seed or fix an artifact's shared data |

## Shared state in pages

```html
<script src="/__apphost/sdk.js"></script>
<script>
  const todos = apphost.collection('todos');
  todos.live(docs => render(docs));            // full list now and after every change by anyone
  await todos.add({ text: 'Ship it', done: false });
  await todos.update(id, { done: true });      // shallow merge
  await todos.remove(id);
  const me = await apphost.me();               // { id, email, name, role }
</script>
```

See [src/guide.rs](src/guide.rs) for the full guide (what Claude reads) and
[examples/todo/index.html](examples/todo/index.html) for a complete page. Limits are 256 KB per
document and 100,000 documents per artifact. Optimistic concurrency is available through
`expectedVersion`.

## Access model

- **Owner:** whoever published the artifact. Manages sharing and visibility, and can delete.
- **Editor:** can publish new versions and roll back.
- **Viewer:** can open the page and read and write its shared data.
- **Visibility:** `private` means only the owner and explicit shares. `org` means everyone who can
  sign in with your Entra tenant.
- Shares target an email address or an Entra **group object id**. Group shares need the groups
  claim (see below).
- Access is re-checked on every page and API request, so revoking a share takes effect immediately.

## Security design

- **Origin isolation.** Every artifact runs on its own origin (`<id>.<content domain>`), so a page
  cannot read another artifact's data or call the portal API with the viewer's session. **Use a
  separate registrable domain for content** (for example `corp-usercontent.example`, not a
  subdomain of the portal's domain), as GitHub does with githubusercontent.com.
- **SSO hand-off.** A content origin without a valid session redirects to the portal. The portal
  checks the Entra session and the artifact ACL, then redirects back with a 60-second signed token.
  That token becomes a host-only, `HttpOnly`, `__Host-` cookie scoped to that one artifact.
- **CSP on artifact pages.** Scripts, styles and fonts come only from the page itself and an
  allowlist of CDNs (configurable). `connect-src 'self'` blocks sending data to external hosts.
  Other headers: `frame-ancestors 'self'`, `object-src 'none'`, `nosniff`, `COOP`/`CORP`, and a
  restrictive `Permissions-Policy`.
- **CSRF.** Cookie-authenticated writes require an `x-apphost-csrf` header. Cross-origin pages
  cannot send it without a CORS preflight, which apphost never grants for cookie endpoints.
- **Tokens.** Sessions, PATs and OAuth tokens are random 256-bit values. Only SHA-256 hashes are
  stored. OAuth uses PKCE S256, single-use 5-minute codes, 1-hour access tokens, and rotating
  30-day refresh tokens. Client registration only accepts allowlisted redirect URIs.
- **Entra.** Authorization-code flow with PKCE, a nonce, and ID token signature, issuer, audience
  and tenant validation against the tenant JWKS.

## Entra app registration

1. Entra admin center → App registrations → New registration. Use single tenant.
2. Platform **Web**, redirect URI `https://apphost.corp.example/auth/callback`.
3. Certificates & secrets → New client secret. Store it in Secrets Manager.
4. API permissions: Microsoft Graph delegated `openid`, `profile`, `email`. Grant admin consent.
5. Optional, for group sharing: Token configuration → Add groups claim → Security groups → ID
   token. Users in more than about 200 groups get no inline claim. Share with individuals or
   smaller groups in that case.
6. Optional: Enterprise applications → apphost → Properties → **Assignment required = Yes**, then
   assign the groups allowed to use apphost.
7. Set `ENTRA_TENANT_ID`, `ENTRA_CLIENT_ID` and `ENTRA_CLIENT_SECRET`.

## Deploying on AWS (ECS Fargate)

- **Image:** `docker build -t apphost .` and push to ECR. It's a single static binary on
  debian-slim and runs as non-root.
- **ECS service:** 2+ Fargate tasks behind an ALB, container port 8080, health check `GET /healthz`
  (it checks DB connectivity). Instances need no sticky sessions. Live updates fan out between
  tasks through Postgres `LISTEN/NOTIFY`.
- **ALB:** one HTTPS listener with two host-header rules, `apphost.corp.example` and
  `*.apphost-usercontent.corp.example`, both forwarding to the same target group. Default idle
  timeout is fine; SSE sends keep-alives every 15 s.
- **DNS/TLS:** Route 53 alias records for both names. ACM certificates for the portal name and the
  wildcard content name.
- **RDS PostgreSQL 14+:** apphost uses `DATABASE_URL` with up to 20 connections per task.
  Back it up; it holds all shared state and metadata.
- **S3:** a private bucket with versioning and SSE-KMS recommended, plus a lifecycle rule for
  noncurrent versions. The task role needs `s3:GetObject`, `s3:PutObject` and `s3:DeleteObject`
  on `arn:aws:s3:::BUCKET/PREFIX*`, and `s3:ListBucket` on the bucket.
- **Secrets:** inject `DATABASE_URL`, `APPHOST_SECRET` and `ENTRA_CLIENT_SECRET` from Secrets
  Manager.
- **Logs:** JSON to stdout (`APPHOST_LOG_JSON=true`, the default in the image), shipped to
  CloudWatch.

All settings are listed in [.env.example](.env.example).

## HTTP API

The portal API (`/api/*`) accepts the session cookie (plus the `x-apphost-csrf` header for writes)
or `Authorization: Bearer <PAT>`. That makes it usable from CI as well:

```bash
curl -H "Authorization: Bearer $APPHOST_TOKEN" -H 'content-type: application/json' \
  https://apphost.corp.example/api/artifacts \
  -d '{"title":"Build report","files":[{"path":"index.html","content":"<h1>OK</h1>"}]}'
```

| Endpoint | |
|---|---|
| `GET /api/artifacts?scope=all\|mine\|shared&q=` | List |
| `POST /api/artifacts` | Publish new (`files[]` with `content` or `content_base64`) |
| `GET/PATCH/DELETE /api/artifacts/{id}` | Details, edit title/description/visibility, delete |
| `POST /api/artifacts/{id}/versions` | Publish a new version (`replace_all`, `remove_paths` supported) |
| `POST /api/artifacts/{id}/rollback` | Restore an earlier version as a new version |
| `POST/DELETE /api/artifacts/{id}/shares` | Add or remove a share |
| `GET /api/artifacts/{id}/data[/{collection}]` | Browse shared data |
| `GET/POST /api/tokens`, `DELETE /api/tokens/{id}` | Personal access tokens |

## Development

```bash
cargo test                 # unit tests
cargo build --release
```

The code is in `src/`:

- `main.rs` handles host-based dispatch.
- `portal.rs`, `content.rs`, `mcp.rs` and `oauth.rs` hold the HTTP surfaces.
- `artifacts.rs` and `data.rs` hold the domain logic.
- `auth.rs`, `entra.rs` and `login.rs` handle identity.
- `storage.rs` covers S3 and the filesystem.

The UI lives in `ui/` (plain JS, no build step) and is embedded in the binary.
