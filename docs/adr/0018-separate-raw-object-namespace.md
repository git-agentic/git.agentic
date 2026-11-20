# ADR-0018: Separate Namespace for Raw Objects

Status: Accepted
Owner: Toni Bergholm
Date: 2026-11-20
Amends: ADR-0002 (object store layout)

## Context

The object store keeps two kinds of content under one address space.
Typed objects (`put`, `get`) are stored as a JSON envelope tagged with their kind.
Raw objects (`put_raw`, `get_raw`), used for memory segments and segment manifests, are stored as the bare bytes.
Blobs hash their bytes directly, so a Blob and a raw object with the same bytes have the same address.
Trees and Commits hash `json(inner)` but are stored in the tagged envelope, so a Blob whose bytes equal that JSON shares an address with the Tree or Commit.

Whoever wrote first owned the address.
The filesystem store skipped a write when the path existed, and the GCS store uploaded unconditionally and could overwrite.
A client that controls prompt blob bytes can predict segment bytes, because they derive from rows it wrote.
It could therefore occupy a raw object's address with a blob envelope.
A later honest `put_raw` then did nothing, and `get_raw` failed its integrity check.
The commit landed but could never be restored.

## Decision 1: raw objects live in their own namespace

Raw objects are stored apart from typed objects, with a distinct suffix, so the two can never share a path or object name, even for stores whose roots or prefixes nest.

- Filesystem: `<root>/raw/<ab>/<rest>.raw`. Typed objects stay at `<root>/<ab>/<rest>.zst`.
- GCS: object name `<prefix>/raw/<a>/<b>.raw.zst`, with the local cache at `<cache_dir>/raw/<a>/<b>.raw.zst`. Typed objects keep their current names.

`get_raw` falls back to the legacy shared path when the raw path is missing, so existing stores keep reading.
A legacy address may hold bare raw bytes or a blob envelope of the same bytes, and both are accepted.
`get` accepts bare raw bytes at a typed address as a Blob with the same content.
Every read is still verified against its address.
`has` is true if either namespace holds the object.

## Decision 2: reject ambiguous blobs

A Blob whose bytes are exactly the canonical JSON of a Tree or a Commit is rejected with `Error::AmbiguousBlob`.
That is the only way a Blob can share an address with a typed object.
The daemon maps it to a validation error (`ambiguous_blob`, not retryable), the same class as `SecretDetected`.

## Decision 3: verify an occupied address, never trust it

A put that finds its address occupied reads the occupant exactly as the matching read would, and classifies it.

- Same content: a dedup hit, nothing is written.
- A valid object of another kind: the put fails with `Error::AddressOccupied` (a validation error on the wire, `address_occupied`, not retryable). Nothing is written or cached.
- Corrupt, meaning the bytes were read but do not decode or verify: the occupant is replaced.

On the filesystem the replacement is atomic (unique temp file, then rename). A read failure other than "not found" is an error, not corruption.
On GCS every upload sends `ifGenerationMatch=0`, so an existing object is never overwritten blindly. HTTP 412 means the name is taken, and the occupant is downloaded and classified.
A corrupt occupant is replaced with `ifGenerationMatch=<generation reported by the download>`. If the download reports no generation, or the object changed meanwhile, the put fails.

## Decision 4: the cache follows the bucket

The GCS local cache is written only once the bucket is known to hold the verified object: after a successful upload, or after a 412 whose occupant classified as the same content.
A failed or refused put leaves no cache entry.
`get_raw` and `has` consult the hash-verified cache under both names before any network request.

## Decision 5: rollback refuses legacy ambiguous blobs up front

A pre-upgrade store can already hold a prompt, tool or model blob whose bytes are canonical Tree or Commit JSON.
Rollback forward-records by re-staging those blobs, which would now fail with `AmbiguousBlob` after migrations, memory restore and prompt writes had already changed live state.
Rollback therefore loads everything it will re-stage right after validating the target, and refuses with `AmbiguousBlob` before any side effect, dry runs included.

## Consequences

- A dedup hit on GCS costs two requests (the conditional upload and a download) where it used to cost one. A new object still costs one.
- Existing stores keep working without migration. New raw objects land in the new namespace, and old ones are read through the legacy fallback.
- Legacy ambiguous blobs stay in the store. They are readable, but a rollback to a commit that references one is refused with a clear error before anything changes.
- A legacy occupant that is a valid object of another kind blocks a later put at its address with a non-retryable `address_occupied`. The realistic case is a pre-upgrade prompt or tool blob whose bytes are the canonical JSON of a predictable Tree: every later commit that produces that Tree fails until the occupant is removed. A bare raw segment or manifest cannot equal Tree or Commit JSON, so it does not cause this.
- Repair: the error names the address. An operator who confirms the occupant is not referenced by a commit they need deletes that one object file (or GCS object) and retries the commit.

## Alternatives considered

Kind-domain-separated addressing, where the kind is part of the hash input, would remove the whole class of collision.
It changes every address and the Commit wire contract, so it needs its own ADR and a store migration, and it is out of scope here.
