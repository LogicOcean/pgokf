# Version History

`pgokf` can keep an **append-only version history** of every concept and answer
point-in-time questions against it - *"what did this runbook say last Tuesday?"*.
The feature is **opt-in and off by default**, so an existing install (and any
bundle synced with history disabled) behaves exactly as before with **zero extra
storage**.

## The switch

History is governed by one durable configuration key, `track_history`
(`boolean`, default `false`):

```sql
-- Start recording (admin-only). Not retroactive: recording begins at the next sync.
SELECT pgokf.set_config('track_history', 'true'::jsonb);

-- Stop recording. Already-recorded history is kept and stays queryable.
SELECT pgokf.set_config('track_history', 'false'::jsonb);
```

While `track_history` is off, no `pgokf.concept_history` row is ever written and
sync behaves byte-for-byte as it did before the feature existed; on an install
where it has never been enabled, the reader functions therefore return no rows.
Enabling it is a **storage/retention tradeoff**; see
[Retention](#retention).

## The temporal model (SCD Type-2)

Each concept accumulates a chain of **versions**. A version has a per-concept
monotonic `version` number and a validity interval `[valid_from, valid_to)`:

- `valid_to IS NULL` marks the **single current open version** of a live concept.
- Intervals are **contiguous and non-overlapping**: the `valid_to` of version *N*
  equals the `valid_from` of version *N+1*.

Every sync records changes from its own delta, inside the same transaction (so
history commits atomically with the sync), stamping all of that sync's rows with
a single captured instant:

| The sync… | records |
| --------- | ------- |
| **adds** a concept | version 1, `change_kind = 'added'`, `valid_from = now`, open (`valid_to = NULL`). |
| **updates** a concept | closes the open version (`valid_to = now`) and appends `version = prev + 1`, `change_kind = 'updated'`, open. |
| **removes** a concept | closes the open version (`valid_to = now`) and appends a **zero-width removal tombstone** (`valid_from = valid_to = now`), `change_kind = 'removed'`. |

Because the sync engine re-parses only content-changed files, an `updated` row
always corresponds to a real change - an unchanged file records nothing. A
removal tombstone carries a `NULL` core snapshot: the last real content stays in
the closed prior version, and the tombstone is purely the deletion marker.

Enabling `track_history` mid-life is safe: a concept first versioned afterward
simply begins its chain at that sync's `change_kind` (its version 1), and the
invariants hold from there forward.

## Reading history

`pgokf.concept_history(bundle_id, concept_id, max_rows DEFAULT 100)` returns the
version timeline, newest first, as `SETOF pgokf.concept_version`:

```sql
SELECT version, change_kind, valid_from, valid_to, title
FROM pgokf.concept_history(1, 'runbooks/database-failover');
--  version | change_kind |       valid_from       |        valid_to        |         title
-- ---------+-------------+------------------------+------------------------+------------------------
--        3 | removed     | 2026-08-27 09:15:00+00 | 2026-08-27 09:15:00+00 |
--        2 | updated     | 2026-08-20 14:02:00+00 | 2026-08-27 09:15:00+00 | Database Failover (v2)
--        1 | added       | 2026-08-13 11:00:00+00 | 2026-08-20 14:02:00+00 | Database Failover
```

`pgokf.concept_as_of(bundle_id, concept_id, as_of)` returns the single version
valid at an instant - the point-in-time answer:

```sql
SELECT version, title, description
FROM pgokf.concept_as_of(1, 'runbooks/database-failover',
                         TIMESTAMPTZ '2026-08-25 00:00:00+00');
```

`concept_as_of` covers `as_of` when `valid_from <= as_of AND (valid_to IS NULL OR
as_of < valid_to)`. Because a removal tombstone is zero-width, an as-of at or
after the removal - and any instant before the concept first existed - returns
**zero rows**.

Both functions are reader-level (`pgokf_reader`), `STABLE`, and run with invoker
rights, so a session's own `pgokf.tenant` [row-level
security](multi-tenancy.md) applies - a scoped session sees only its own history.

## Retention

`history_retention_days` (`integer`, default `0` = keep indefinitely) bounds
growth. When it is positive and `track_history` is on, **closed** versions
(`valid_to IS NOT NULL`) older than `now() - history_retention_days` are pruned
in the same transaction after each sync. The **current open version** of each
concept (`valid_to IS NULL`) is **never** pruned, so present-time point-in-time
queries always resolve.

```sql
SELECT pgokf.set_config('history_retention_days', '90'::jsonb);  -- keep 90 days of closed versions
SELECT pgokf.set_config('history_retention_days', '0'::jsonb);   -- keep forever (default)
```

## Storage notes

- History lives in **`pgokf.concept_history`**, cascading from `pgokf.bundles`
  (not `pgokf.concepts`) - a removed concept keeps its history until the bundle is
  unregistered.
- Each version snapshots the concept core (`type`, `title`, `description`, `tags`,
  `resource`, `body_text`, `file_hash`). Enable history only where the audit trail
  is worth the storage, and use `history_retention_days` to cap it.

See [Configuration](configuration.md#version-history-track_history-history_retention_days)
for the keys and [SQL API](sql-api.md#version-history-opt-in) for the full
function and table reference.
