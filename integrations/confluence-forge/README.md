# Confluence Forge app: pushed events

A private [Forge](https://developer.atlassian.com/platform/forge/) app that
makes Confluence Cloud push page, comment, and task events to a pgokf
deployment as they happen. No polling: Confluence sends each event, through
[Forge Remote](https://developer.atlassian.com/platform/forge/remote/), to the
`pgokf-confluence-events` receiver in the companions image.

The app is only a [manifest](manifest.yml). It ships no functions or
dependencies; Forge delivers events straight to the receiver.

```text
Confluence Cloud ──event──▶ Forge ──HTTPS + invocation token──▶ pgokf-confluence-events
```

## What it sends

| Events | Why |
| --- | --- |
| Page created, updated, moved, copied, archived, unarchived, trashed, restored, deleted | Keep the catalog's copy of each page current |
| Page permissions updated | A page whose audience narrows must stop being served |
| Comment created, updated, deleted | Discussion on a page, including replies to an agent's challenge |
| Task created, updated, removed | An approver ticking an "Approve" action item |

Each event names the page, its version, and the account that caused it.
Events the app causes itself (its own writes) arrive marked `selfGenerated`
and the receiver drops them. Forge's `ignoreSelf` filter works only for Jira
events, so the manifest cannot filter them.

The app asks for these scopes:

| Scope | Why |
| --- | --- |
| `read:app-system-token` | A token with each event, so the receiver can read Confluence as the app |
| `read:confluence-content.summary` | Page, comment, and permission events |
| `read:confluence-content.all` | Task events |
| `write:confluence-content` | Required by Forge for trashed and deleted page events; also for publishing approved changes |
| `read:email-address:confluence` | Record the acting person as `human:<email>` |

## Deploy

You need Node.js 22.23.4+ or 24.22.1+ and the Forge CLI
(`npm install -g @forge/cli`). Test on a free
[Confluence developer site](https://go.atlassian.com/cloud-dev) before your
organization's site. Installing on a site needs a site administrator.

1. **Run the receiver** where Forge can reach it over HTTPS, behind a
   TLS-terminating proxy or ingress:

   ```bash
   FORGE_APP_ID=ari:cloud:ecosystem::app/<uuid> \
   CONFLUENCE_CLOUD_ID=<cloud-id> \
   pgokf-confluence-events --listen 0.0.0.0:8090
   ```

   The site's cloud ID is at `https://<site>.atlassian.net/_edge/tenant_info`.
   `GET /healthz` answers without a token. Events arrive at
   `POST /forge/confluence/events`.

2. **Register the app** from a copy of this directory. Log in with your
   Atlassian account and an unscoped
   [API token](https://id.atlassian.com/manage-profile/security/api-tokens).
   Where `forge login` can't run, such as a server without a browser or CI,
   set `FORGE_EMAIL` and `FORGE_API_TOKEN` instead. Forge registers apps into
   a [Developer Space](https://developer.atlassian.com/platform/forge/developer-space/developer-spaces-introduction/);
   if the list below is empty, create one in the Developer Console, because
   the CLI can't.

   ```bash
   forge login
   forge developer-spaces list
   forge register --developer-space-id <space-id> "pgokf Confluence events"
   ```

   `forge register` writes your app ID into `manifest.yml`. Use it as
   `FORGE_APP_ID` in step 1. The ID in this repository is a placeholder;
   don't commit your own.

3. **Point the app at the receiver.** Set `remotes[0].baseUrl` in
   `manifest.yml` to the receiver's public HTTPS origin, for example
   `https://pgokf-events.example.com`. Do this before installing: the site
   administrator approves this address at install, and changing it later
   takes `forge install --upgrade` and their approval again.

4. **Deploy and install.** Use the `development` environment for a test
   site:

   ```bash
   forge lint
   forge deploy --environment production
   forge install --environment production --site <site>.atlassian.net --product confluence
   ```

5. **Check.** Edit a page. Within a few minutes the receiver writes one JSON
   line describing the change. Its log names any delivery it refused, and
   why.

## How deliveries are trusted

The receiver accepts a delivery only when its bearer token:

- is an RS256 JWT that verifies against
  [Atlassian's published keys](https://forge.cdn.prod.atlassian-dev.net/.well-known/jwks.json);
- was issued by `forge/invocation-token` and has not expired;
- names the configured app ID, in both its audience and its `app.id` claim;
- names the configured Confluence site, and no other, wherever it names a
  site (its invocation context and its installation's contexts).

It refuses Isolated Cloud tokens and tokens signed with any other algorithm,
and answers every refusal of a token with a `401` and a `Bearer` challenge.
Keys are read again only when a token names a key the receiver has not seen,
at most once a minute; a delivery signed with a key that cannot be read yet is
answered `503`, so Forge retries it. A body declared as anything but JSON is
refused, and a page, space, or comment named by anything but a canonical
Confluence ID makes the event malformed. The app system token is passed to
the receiver's sink and is never logged, and refusal reasons are logged with
control characters escaped, one line each.

## Limits

- Forge may take **up to 3 minutes** to deliver an event.
- Forge retries a failed delivery up to 4 times, 60 seconds apart. Order and
  exactly-once delivery are not documented, so consumers deduplicate on each
  event's key and order by page version.
- Events larger than 200 kB are not delivered.
- For now the receiver writes accepted events to standard output as JSON
  lines. Recording them in the catalog comes next.
