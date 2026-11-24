# OSC browser sign-in for a native app: refresh, client_id, workspaces

Findings for #74 (OL-62a), AC0 of #65 (OL-62). Researched 2026-11-17 by reading source and
the live discovery document. No sign-in or token exchange was attempted.

## Answer (AC0)

**Yes.** Open Source Cloud offers a browser sign-in a native app can use (OAuth 2.0 authorization
code + PKCE S256 with a loopback redirect, RFC 8252), and it can be refreshed.

| Question | Answer |
|---|---|
| 1. Does `POST /api/connect/token` return `refresh_token`, and does it accept `grant_type=refresh_token`? | **Yes, and yes.** Every successful grant returns `access_token` (1 h) and `refresh_token` (30 days). A refresh returns a new pair, so the 30 days slide. |
| 2. A separate public client_id for Open Live Ingest, or reuse `cli`? | **Allowed, not required.** `cli` works for any loopback port with no secret. A separate public client_id is available today, without the OSC team, through dynamic client registration (`POST /api/connect/register`, PKCE only, no secret). A fixed first-party id needs an OSC change (optional ask below). |
| 3. An API listing the workspaces (tenants) a user belongs to? | **No.** The workspace is chosen on the OSC consent page, and every token is bound to that one workspace (`tenantId` claim). Switching means signing in again. |

## Sources

Verified against the OSC auth server source (private; `osaas-auth` @ `ef3b637`, `osaas-app` @
`7f5b03d`) on 2026-11-17. Only behaviour a client can observe through the public endpoints on
`app.osaas.io` is recorded here.

Public sources, readable without credentials:

- `GET https://app.osaas.io/.well-known/oauth-authorization-server` (RFC 8414) returns 200 with
  `authorization_endpoint` `/api/connect/authorize`, `token_endpoint` `/api/connect/token`,
  `registration_endpoint` `/api/connect/register`, `code_challenge_methods_supported: ["S256"]`,
  and `grant_types_supported: ["authorization_code", "client_credentials"]`.
  `GET /.well-known/openid-configuration` returns 404: it is OAuth 2.0, not OpenID Connect (no
  id_token, no userinfo).
- [client-ts `packages/cli/src/login.ts`](https://github.com/EyevinnOSC/client-ts/blob/08056941c2a558ae01b40563abeb5beb2fc8c706/packages/cli/src/login.ts)
  @ `0805694` (added in 4ec1a77, #93): `CLIENT_ID = 'cli'` (L9), authorize and token URLs
  (L60-61), loopback server on port 0 with `/callback` (L66-69, L147-156), PKCE S256 (L40-46,
  L161-162). It reads only `access_token` from the response (L126-129) and drops the
  `refresh_token`; that, not the server, is why the CLI login lasts an hour.
- [terraform-provider-osc `internal/provider/provider.go`](https://github.com/EyevinnOSC/terraform-provider-osc/blob/136ef743de263404db3eabad623aadebe2478d0f/internal/provider/provider.go)
  @ `136ef74`, L67-73: "Every OSC access token is bound to one workspace".

### 1. Refresh

- The token endpoint accepts `grant_type=authorization_code` and `grant_type=refresh_token`;
  anything else is `unsupported_grant_type`.
- Both grants answer `{access_token, refresh_token, token_type: "Bearer", expires_in: 3600,
  scope: "claudeai"}`. The access token lasts 1 hour, the refresh token 30 days.
- A refresh is `grant_type=refresh_token&refresh_token=...`; no `client_id` or secret is needed.
- An invalid, expired or revoked refresh token gives `400 invalid_grant`. A suspended workspace
  gives `403 access_denied`. A temporary server-side failure (for example while checking the
  workspace or its plan) gives `503`: retry, do not ask the person to sign in again.
- OSC's own connect documentation still says the refresh token lasts 2 hours; the server issues
  30-day refresh tokens. Go by the `expires_in` and the token, not the document.
- The discovery document omits `refresh_token` from `grant_types_supported` (and lists
  `client_credentials`, which the token endpoint refuses). Do not use it to detect refresh support.

### 2. client_id

- `cli` (shown as "OSC CLI" on the consent page) is accepted with `http:`/`https:` redirects on
  `localhost`, `127.0.0.1` or `[::1]`, any port and path, and needs no secret: do not send one.
- Dynamic client registration (RFC 7591): `POST /api/connect/register` with
  `{client_name, redirect_uris}` returns a `client_id` with
  `grant_types: ["authorization_code", "refresh_token"]` and `token_endpoint_auth_method: "none"`.
  `http:` redirects are allowed for loopback hosts only. For loopback redirects the port is
  ignored when matching (RFC 8252 §7.3), so one registration covers any `127.0.0.1:0` listener.
  A registered client does not send a secret. Registrations are not permanent: plan for one to
  expire after about 30 days.
- OAuth apps created inside a workspace require a `client_secret`, which a distributed desktop
  binary cannot keep, so that route is out.
- The consent page shows the client's display name. For `cli` and dynamically registered clients
  the page after approval also shows AI-assistant example prompts.

**Recommendation for #75:** register dynamically with `client_name: "Open Live Ingest"` and
`redirect_uris: ["http://127.0.0.1/callback"]`, cache the `client_id` with its issue time (it is
not a secret; the settings file or the keychain entry is fine), and register again when it is
older than about 25 days or the authorize step fails for an unknown client. The person then sees
"Open Live Ingest" on the consent page instead of "OSC CLI". Keep `cli` as the fallback if
registration fails: same flow, same tokens, only the displayed name differs. Either way, bind the
listener to `127.0.0.1:0` and send the same `redirect_uri=http://127.0.0.1:<port>/callback` on both
the authorize and the token request. Refresh does not depend on the client registration, so an
expired registration does not break an existing refresh token.

### 3. Workspaces

- The consent page lists the user's workspaces from the browser session (a selector only when
  there is more than one) and issues the code for the chosen one. The tokens carry that
  `tenantId` and nothing about the user's other workspaces.
- No endpoint lists a user's workspaces for a bearer token. `GET /api/workspace/members` lists the
  members of the token's own workspace, not the user's workspaces. The Terraform provider (above)
  states the same one-workspace binding and has no list either.
- This matches `docs/DESIGN.md` ("the platform offers no way to list or switch workspaces from
  outside").

**Consequence for #77:** take the fallback path. Show the workspace from the token's `tenantId`
(`osc::inspect`), and offer "Change workspace" as a new browser sign-in, where the person picks
another workspace on the consent page. Signing in again gives a new refresh token for the new
workspace; replace the stored one.

## Consequences for the sibling issues

- **#75:** implement the flow above natively (no Node). Keep the `refresh_token`, unlike the CLI.
- **#76:** store the refresh token in the keychain. It is a 30-day credential, at least as
  sensitive as a personal access token, and never goes to the settings file or the logs. Refresh
  the access token a few minutes before its `expires_in` and store the newest refresh token after
  every refresh, so a 4-hour show (AC2) needs no new sign-in. On `invalid_grant` or `403`, raise
  the "sign in again" event; on `503`, retry. The "cannot be refreshed" event stays for a refresh
  token that is revoked or older than 30 days.
- **#77:** no workspace list; re-sign-in to switch (above).
- **#65 AC1:** the "yes" path applies. No request to the OSC team is needed to ship.

## Optional asks to the OSC team (not filed)

Nothing blocks #65. These would improve it, and are to be raised with the OSC team through their
own channel if wanted:

1. A first-party, loopback-only client id for Open Live Ingest, shown as "Open Live Ingest" on the
   consent page, without the AI-assistant example prompts after approval.
2. A workspace list for an OAuth access token (for example `[{id, name}]` for the token's user), so
   the app can show the workspace by name and offer only real choices.
3. Advertise `refresh_token` in `grant_types_supported` of the discovery document.
4. Fix the refresh-token lifetime in the OSC connect documentation (30 days, not 2 hours).
