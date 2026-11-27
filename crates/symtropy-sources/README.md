# symtropy-sources

Headless, user-initiated remote source adapters for Symtropy.

This crate intentionally does **not** poll feeds, crawl tags, or write into the shared library index. A lookup starts from an explicit user action, and a remote item only enriches local metadata after its pixels have been matched against the local file.

## Flow

```text
local tags/features
  → user chooses a site or pastes a URL
  → worker fetches RemoteItem metadata and its preview
  → visual match (dhash + dimensions) produces MatchEvidence
  → user stages a freshly generated src:<site>:<tag> patch for review
  → separate user action downloads the full file to the local source cache
  → review UI decides what becomes canonical
```

Rejected matches produce no proposal operations. Confirmed matches never create accepted tags and never remove local tags. The UI runs lookups only from a button click, polls results outside the viewer's metadata snapshot, and never stages a proposal automatically; it regenerates patches against the current tag-store revision when the user chooses a candidate.

A separate Attach Source editor records a pasted URL as sidecar provenance. It performs no lookup, creates no tag assertions, and accepts any HTTP/HTTPS link; unknown hosts are labelled `manual`.

When a local image is opened, source URLs already present in its sidecar are captured into the review store as suggested `evidences` relations. Capture is deduplicated, makes no network request, and never replaces accepted or locked claims.

## Crate shape

- `model.rs`: `RemoteItem`, `SearchQuery`, `Fetcher`, `SourceAdapter`, site enum, and adapter readiness capabilities.
- `http.rs`: shared blocking HTTP client with descriptive user agent, HTTP-only URL validation, request spacing, body cap, timeout, and bounded retry/backoff.
- `policy.rs`: per-run budget, match thresholds, allowed-site list, backoff, and request counter.
- `adapters.rs`: e621 implementation plus explicit stubs for Met, Openverse, Tumblr, and Fur Affinity.
- `lookup.rs`: one-click orchestration from local tags to scored candidates, plus explicit download/ingest.
- `matching.rs`: perceptual-hash comparison and conversion to `symtropy-tags` proposals.
- `ingest.rs`: local cache materialization with metadata-only sidecars; byte-identical rewrites are skipped.

Adapters never touch disk. `Fetcher` is injected, so tests use recorded responses and never touch the network.

## Site notes

- e621: public API, tag-rich, about 2 requests/second. The shared client spaces e621 requests by 500 ms by default and sends a descriptive `User-Agent`.
- Tumblr: API v2 is feasible with a user-supplied key. Pasted post URLs are preferred; no scraping fallback.
- Fur Affinity: official content API is beta/rolling. Pasted links only for now.
- Met/Openverse: preferred legal seed sources, enabled only with an explicit curated query plan.
- Twitter/X: no adapter; link-only/deferred pending API strategy and explicit approval.

## Cache layout

`Ingestor::new(cache_dir)` writes to:

```text
<cache_dir>/sources/<site>/<slug-id>-<content-hash>.<detected-image-extension>
<cache_dir>/sources/<site>/<slug-id>-<content-hash>.json
```

The JSON sidecar contains provenance only: remote id, site, source URL, title, author, description, capture time, and source tags. Third-party image bytes remain in the local cache and are never part of the shared metadata index.
