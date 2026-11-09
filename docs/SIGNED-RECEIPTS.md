# Signed receipt batches

Export signs the receipt references already recorded for one conformance case. It reads
`<Tempo home>/conformance/<CASE>/signed-receipts.json`, which must contain exactly
`{"schema_version":1,"records":[...]}`. Each row has only `receipt_id` and
`artifact_digest`. IDs are 1–64 ASCII letters, digits, underscores or hyphens; digests are
64 lowercase hex characters. A batch has 1–1000 rows and its input and output are capped at
1 MiB.

```text
tempo-launch receipts export --case CASE --out NEW_FILE
tempo-launch verify --signed FILE --trusted-key PUBLIC_HEX
```

Export sorts rows by `receipt_id`, then signs the compact payload
`{"schema_version":1,"case_id":CASE,"records":[...]}` preceded by the domain
`perennos-tempo.receipt-batch.v1\0`. The envelope fields are `schema_version`, `case_id`,
`records`, `node_public_key`, and `signature`, in that order. Export reads only the existing
32-byte `<Tempo home>/fleet-identity`; it never creates or rotates a key. On Unix that file
must have owner-only permissions. Output must be a new file in an existing safe folder; creation
uses mode `0600` on Unix and never replaces an existing file.

Verify requires the public key supplied separately from an authenticated Fleet pairing or
another trusted Fleet record. The envelope's own key is not a trust source. Verification
checks the strict Ed25519 signature over the normalized batch and can bind it to an expected
case through the library API.

A valid signature means the Fleet key sealed this export. It does not prove who authored the
original work, when the references were created, that the batch is complete, human authority,
or legal conformance. This does not change the existing Python SHA-256 chain; that chain and
this signed export answer different questions.
