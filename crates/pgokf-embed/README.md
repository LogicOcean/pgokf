# pgokf-embed

The reference **embedding-generation companion** for [`pgokf`](../extension).

`pgokf` ships semantic search — `pgokf.concept_search_semantic(real[])`,
`pgokf.concept_search_hybrid(...)`, and the `pgokf.concept_embedding` store — but
it deliberately **never computes an embedding and never performs network I/O**.
Vectors are streamed in from outside through
`pgokf.set_concept_embedding(bundle_id, concept_id, embedding)`. `pgokf-embed` is
that outside half: the reference embedder that pairs with the shipped search.

It is a small, standalone async binary that:

1. connects to PostgreSQL as a `pgokf_writer`-capable role;
2. finds every concept in `pgokf.concepts` that has **no** matching
   `pgokf.concept_embedding` row (optionally scoped to one `--bundle`);
3. builds a bounded input text per concept — `title + description + body_text`,
   truncated to `--max-chars` on a UTF-8 boundary;
4. calls a configurable **OpenAI-compatible** embeddings endpoint
   (`POST {endpoint}/v1/embeddings` with `{"model": ..., "input": [...]}`,
   `Authorization: Bearer <key>`), in batches of `--batch-size`;
5. streams each returned vector back with `pgokf.set_concept_embedding`.

## Where credentials live

The embeddings endpoint URL, model name, and API key are supplied on the CLI or
through the environment, and are **never hard-coded and never written to
PostgreSQL**. The database itself never learns the endpoint or the key — it only
ever receives finished vectors through `pgokf.set_concept_embedding`. The
PostgreSQL connection string authenticates a login role that is a member of
`pgokf_writer` (the tier the setter requires); reading `embedding_dim` and the
concept projections additionally needs `pgokf_reader`.

## Any OpenAI-compatible endpoint

The `/v1/embeddings` protocol is shared by OpenAI and by every drop-in
compatible server, so the same binary works against:

- **OpenAI** — `--endpoint https://api.openai.com --model text-embedding-3-small
  --api-key sk-...`;
- a **local** [`text-embeddings-inference`](https://github.com/huggingface/text-embeddings-inference)
  or [`llama.cpp`](https://github.com/ggml-org/llama.cpp) server —
  `--endpoint http://127.0.0.1:8080 --model <model>` (no key needed);
- a **mock** server that returns a deterministic vector per input, used in the
  end-to-end test below.

The embedding **dimension** must match the catalog's durable `embedding_dim`
configuration key: `pgokf-embed` reads it from `pgokf.get_config()` by default,
or you can pin it with `--dim`. The setter rejects any vector of the wrong
length, so a mismatch fails loudly.

## Usage

```
pgokf-embed \
  --database-url "postgresql://okf_embed@localhost/app" \
  --endpoint https://api.openai.com \
  --model text-embedding-3-small \
  --api-key "$OPENAI_API_KEY" \
  --bundle 1 \          # optional: only this bundle
  --batch-size 32 \     # concepts per HTTP request
  --max-chars 8000      # per-concept input bound
```

Every flag has an environment-variable equivalent:

| Flag | Env | Meaning |
| --- | --- | --- |
| `--database-url` | `OKF_PG_URL` | PostgreSQL URL for a `pgokf_writer` role (required) |
| `--endpoint` | `OKF_EMBED_ENDPOINT` | Base URL of the embeddings server (required) |
| `--model` | `OKF_EMBED_MODEL` | Model name sent in the request body (required) |
| `--api-key` | `OKF_EMBED_API_KEY` | Bearer token (optional for local servers) |
| `--bundle` | `OKF_EMBED_BUNDLE` | Restrict to one bundle id |
| `--dim` | `OKF_EMBED_DIM` | Override the target dimension (default: `embedding_dim`) |
| `--batch-size` | `OKF_EMBED_BATCH` | Concepts per HTTP request (default 32) |
| `--max-chars` | `OKF_EMBED_MAX_CHARS` | Per-concept input character bound (default 8000) |
| `--tenant` | `OKF_TENANT` | Apply a `pgokf.tenant` scope for the session |

After a run, `pgokf.concept_embedding` is populated and
`pgokf.concept_search_semantic(query_embedding)` returns ranked hits (compute the
query vector with the same endpoint/model to keep the space consistent).

## Testing against a mock endpoint

Because the space only has to be internally consistent for an end-to-end test,
you can point `pgokf-embed` at a tiny mock that answers `POST /v1/embeddings`
with a **deterministic** vector per input (for example a fixed-dimension hash of
the text). Run the mock, set the catalog's `embedding_dim` to the mock's
dimension, run `pgokf-embed` against it, and confirm `pgokf.concept_embedding`
fills up and `pgokf.concept_search_semantic` then returns hits. The client is
structured so this mock end-to-end is straightforward: only the base URL, model,
and (optional) key change.
