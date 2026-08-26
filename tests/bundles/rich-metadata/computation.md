---
type: Reference
title: Monthly active account SQL
---

# Computation

```sql
SELECT count(DISTINCT account_id)
FROM account_events
WHERE occurred_at >= :month_start
  AND occurred_at < :month_start + INTERVAL '1 month';
```
