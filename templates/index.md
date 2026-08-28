---
# A bundle-root index.md is a RESERVED OKF file: it is NOT ingested as a
# concept. Its frontmatter may carry ONLY an optional okf_version, which the
# catalog reads and stores on pgokf.bundles.okf_version for the bundle this
# file roots. (log.md is the other reserved name at every directory level —
# chronological bundle/directory history.) Both `okf_version: "0.2"` and the
# unquoted `okf_version: 0.2` are accepted; an absent or malformed value
# leaves pgokf.bundles.okf_version NULL and never aborts a sync.
okf_version: "0.2"
---

# Payments knowledge base

This file documents the bundle for humans. The catalog reads only the
`okf_version` above from its frontmatter — everything else here is ignored by
the parser, so use the body freely for a table of contents or an overview of
the bundle. Because `index.md` is reserved, it produces no `pgokf.concepts`
row; put concept content in any other `.md` file.
