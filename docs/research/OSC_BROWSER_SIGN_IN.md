# OSC browser sign-in for a native app: refresh, client_id, workspaces

Findings for #74 (OL-62a), AC0 of #65 (OL-62). Researched 2026-11-17 by reading source and
the live discovery document. No sign-in or token exchange was attempted.

## Answer (AC0)

**Yes.** Open Source Cloud offers a browser sign-in a native app can use (OAuth 2.0 authorization
code + PKCE S256 with a loopback redirect, RFC 8252), and it can be refreshed.

| Question | Answer |
|---|---|
| 1. Does `POST /api/connect/token` return `refresh_token`, and does it accept `grant_type=refresh_token`? | **Yes, and yes.** Every successful grant returns `access_token` (1 h) and `refresh_token` (30 days). A refresh returns a new pair, so the 30 days slide. |
| 2. A separate public client_id for Open Live Ingest, or reuse `cli`? | **Allowed, not required.** `cli` works for any loopback port with no secret. A separate public client_id is available today, without the OSC team, through dynamic client registration (`POST /api/connect/register`, PKCE only, no secret). A fixed first-party id needs an OSC change (request below, optional). |
| 3. An API listing the workspaces (tenants) a user belongs to? | **No.** The workspace is chosen on the OSC consent page, and every token is bound to that one workspace (`tenantId` claim). Switching means signing in again. |

## Sources

The authorization server is `Eyevinn/osaas-auth` (extracted from `Eyevinn/osaas-app` per its
ADR-0070, same public URLs on `app.osaas.io`, same MongoDB and token secret). `osaas-app` keeps an
identical copy of the routes until that migration finishes. Both repositories are **private**:
the links below resolve only for Eyevinn organisation members. Pinned commits:

- `Eyevinn/osaas-auth` @ `ef3b63790f76e72e1d72030b62ee96b27c742bff` (main, 2026-11-17)
- `Eyevinn/osaas-app` @ `7f5b03de1c4982fd8f9c21151da09365c53fc68e` (main, 2026-11-17)
- `EyevinnOSC/client-ts` @ `08056941c2a558ae01b40563abeb5beb2fc8c706` (main, public)
- `EyevinnOSC/terraform-provider-osc` @ `136ef743de263404db3eabad623aadebe2478d0f` (main, public)

Public, read without credentials:

- `GET https://app.osaas.io/.well-known/oauth-authorization-server` (RFC 8414) returns 200 with
  `authorization_endpoint` `/api/connect/authorize`, `token_endpoint` `/api/connect/token`,
  `registration_endpoint` `/api/connect/register`, `code_challenge_methods_supported: ["S256"]`,
  and `grant_types_supported: ["authorization_code", "client_credentials"]`.
  `GET /.well-known/openid-configuration` returns 404: it is OAuth 2.0, not OpenID Connect (no
  id_token, no userinfo).
- [client-ts `packages/cli/src/login.ts`](https://github.com/EyevinnOSC/client-ts/blob/08056941c2a558ae01b40563abeb5beb2fc8c706/packages/cli/src/login.ts)
  (added in 4ec1a77, #93): `CLIENT_ID = 'cli'` (L9), authorize and token URLs (L60-61), loopback
  server on port 0 with `/callback` (L66-69, L147-156), PKCE S256 (L40-46, L161-162). It reads
  only `access_token` from the response (L126-129) and drops the `refresh_token`; that, not the
  server, is why the CLI login lasts an hour.

### 1. Refresh

- [osaas-auth `src/routes/token.ts`](https://github.com/Eyevinn/osaas-auth/blob/ef3b63790f76e72e1d72030b62ee96b27c742bff/src/routes/token.ts):
  - L115-129: only `authorization_code` and `refresh_token` are accepted; anything else is
    `unsupported_grant_type`.
  - L233-276 `mint`: both grants answer `{access_token, refresh_token, token_type: "Bearer",
    expires_in: 3600, scope: "claudeai"}`. Access token lifetime 3600 s (L247), refresh token
    `3600 * 24 * 30` s (L254).
  - L278-405 `handleRefresh`: the request is `grant_type=refresh_token&refresh_token=...`; no
    `client_id` or secret is read. The refresh token is a signed JWT (L291-292). Refused with
    `400 invalid_grant` when expired, invalid, fine-grained, or revoked (L293-360). A suspended
    workspace gets `403 access_denied` (L361-368). A plan-service failure gives
    `503 temporarily_unavailable` (L369-395): retry, do not ask the person to sign in again.
  - The old refresh token is not invalidated by a refresh; it lives until its own expiry or a
    revocation.
  - The refreshed access token's client is taken from the refresh token's `oauthClientId` claim,
    falling back to `"unknown"` (L403). The refresh token is minted without a client id (L249-255),
    so after the first refresh OSC's audit trail will most likely show `unknown`, not the app.
    Cosmetic for us.
- [osaas-app `src/app/api/connect/token/route.ts`](https://github.com/Eyevinn/osaas-app/blob/7f5b03de1c4982fd8f9c21151da09365c53fc68e/src/app/api/connect/token/route.ts)
  L85, L96-271, L248-261: the same behaviour in the copy osaas-app still carries.
- [osaas-app `docs/oauth-connect.md`](https://github.com/Eyevinn/osaas-app/blob/7f5b03de1c4982fd8f9c21151da09365c53fc68e/docs/oauth-connect.md)
  L88-126 documents the refresh grant, but L150-151 says the refresh token lasts 2 hours. The code
  says 30 days; the document is stale (last changed bbd5002, 2026-05-25).
- The discovery document omits `refresh_token` from `grant_types_supported`
  ([osaas-auth `src/routes/discovery.ts`](https://github.com/Eyevinn/osaas-auth/blob/ef3b63790f76e72e1d72030b62ee96b27c742bff/src/routes/discovery.ts)
  L8-15, kept byte-identical to osaas-app on purpose). Do not use it to detect refresh support.

### 2. client_id

- [osaas-auth `src/routes/authorize.ts`](https://github.com/Eyevinn/osaas-auth/blob/ef3b63790f76e72e1d72030b62ee96b27c742bff/src/routes/authorize.ts):
  - L41-60 `KNOWN_CLIENTS`: `claude`, `chatgpt`, `mcp`, `cli`. `cli` (display name "OSC CLI")
    accepts `http:`/`https:` on `localhost`, `127.0.0.1` or `[::1]`, any port and path.
  - L171-183: a known client is accepted on that rule alone; L186-202: a dynamically registered
    client must match one of its registered redirect URIs, port ignored for loopback (RFC 8252
    §7.3, L107-150); L205-217: a tenant-created OAuth app.
- token.ts L18, L149-186: the known clients need no secret (`OSC_CONNECT_CLIENT_SECRET` is unset in
  production per the osaas-auth README, so a secret must not be sent). Dynamically registered
  clients must not send a secret. Tenant-created OAuth apps require a `client_secret`, which a
  distributed desktop binary cannot keep, so that route is out.
- [osaas-auth `src/routes/register.ts`](https://github.com/Eyevinn/osaas-auth/blob/ef3b63790f76e72e1d72030b62ee96b27c742bff/src/routes/register.ts)
  (RFC 7591): `POST {client_name, redirect_uris}` returns a `client_id` with
  `grant_types: ["authorization_code", "refresh_token"]` and `token_endpoint_auth_method: "none"`
  (L132-134). `http:` is allowed for loopback hosts only
  ([`src/redirectUri.ts`](https://github.com/Eyevinn/osaas-auth/blob/ef3b63790f76e72e1d72030b62ee96b27c742bff/src/redirectUri.ts)
  L25-30). Registrations named like `scanner deployment`, or with a loopback redirect on port
  18863 or 18864, are refused (L108; README).
- [osaas-auth `src/store.ts`](https://github.com/Eyevinn/osaas-auth/blob/ef3b63790f76e72e1d72030b62ee96b27c742bff/src/store.ts)
  L106: a registered client expires after 30 days.
- The consent page shows the client's display name. For known and dynamically registered clients
  the page after approval adds AI-assistant example prompts (`isAiAgent: true`, authorize.ts L183,
  L202, L686).

**Recommendation for #75:** register dynamically with `client_name: "Open Live Ingest"` and
`redirect_uris: ["http://127.0.0.1/callback"]`, cache the `client_id` with its issue time (it is
not a secret; the settings file or the keychain entry is fine), and register again when it is
older than about 25 days or the authorize step fails for an unknown client. The person then sees
"Open Live Ingest" on the consent page instead of "OSC CLI". Keep `cli` as the fallback if
registration fails: same flow, same tokens, only the displayed name differs. Either way, bind the
listener to `127.0.0.1:0` and send `redirect_uri=http://127.0.0.1:<port>/callback` on both the
authorize and the token request (the server rewrites `127.0.0.1` to `localhost` when it stores and
matches the code, store.ts L97-99, L150, L167).

### 3. Workspaces

- authorize.ts L471-478 and L614-619: the consent page lists the user's workspaces from the
  browser session (selector only when there is more than one) and issues the code for the chosen
  one. The tokens carry that `tenantId` and nothing else about the user's other workspaces.
- No endpoint lists a user's workspaces for a bearer token. In osaas-app, the tenant listings are
  under `/api/admin/*` (platform admin only); `GET /api/workspace/members`
  ([route.ts](https://github.com/Eyevinn/osaas-app/blob/7f5b03de1c4982fd8f9c21151da09365c53fc68e/src/app/api/workspace/members/route.ts)
  L13-35) lists the members of the token's own workspace, not the user's workspaces.
- [terraform-provider-osc `internal/provider/provider.go`](https://github.com/EyevinnOSC/terraform-provider-osc/blob/136ef743de263404db3eabad623aadebe2478d0f/internal/provider/provider.go)
  L67-73: "Every OSC access token is bound to one workspace", and the provider has no list either.
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
  the access token a few minutes before its `expires_in` and store the new refresh token each
  time, so a 4-hour show (AC2) needs no new sign-in. On `invalid_grant` or `403`, raise the
  "sign in again" event; on `503`, retry. The "cannot be refreshed" event stays for a refresh
  token that is revoked or older than 30 days.
- **#77:** no workspace list; re-sign-in to switch (above).
- **#65 AC1:** the "yes" path applies. No request to the OSC team is needed to ship.

## Optional request to the OSC team (not filed)

Nothing blocks #65. These would improve it; the text below is ready to file in the OSC
organisation if the team wants them.

> **Title:** OSC Connect: first-party client id for Open Live Ingest, a workspace list for
> OAuth tokens, and two doc fixes
>
> Open Live Ingest (Eyevinn/open-live-ingest) is adding "Sign in with Open Source Cloud" to its
> desktop app with the OSC Connect authorization code + PKCE flow and a loopback redirect, the
> same way `@osaas/cli login` works, and keeps the refresh token so a multi-hour live show stays
> signed in. It works today. We would like:
>
> 1. **A known client `open-live-ingest`** in `KNOWN_CLIENTS` (osaas-auth
>    `src/routes/authorize.ts`, and `ALLOWED_CLIENT_IDS` in `src/routes/token.ts`), loopback only
>    like `cli`, display name "Open Live Ingest", without the AI-assistant example prompts after
>    approval. Today we use dynamic client registration, which expires after 30 days and shows
>    those prompts, or `cli`, which shows "OSC CLI" to a venue operator.
> 2. **A workspace list for an OAuth access token**, for example `GET /api/user/workspaces` with
>    `Authorization: Bearer <access token>` returning `[{id, name}]` for the token's user, so the
>    app can show the workspace by name and offer only real choices. Switching can still go
>    through a new sign-in.
> 3. **Advertise `refresh_token`** in `grant_types_supported` of
>    `/.well-known/oauth-authorization-server` (it lists `client_credentials`, which the token
>    endpoint refuses, and omits `refresh_token`, which it accepts).
> 4. **Fix `docs/oauth-connect.md`** in osaas-app: the refresh token lasts 30 days in the code, not
>    2 hours.
>
> Also worth a look: a refreshed access token carries `oauthClientId: "unknown"`, because the
> refresh token is minted without the client id (token.ts L249-255, L403).
