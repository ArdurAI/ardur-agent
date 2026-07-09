# Signed marketplace capability manifests

Ardur marketplace packages can be distributed as signed capability manifests for either a `skill` or a `plugin`.

A manifest is a JSON file with:

- `schema_version`: currently `1`
- `kind`: `skill` or `plugin`
- `id`, `name`, `version`
- `capabilities`: capability strings the package requests, such as `cap.http.fetch`
- `artifacts`: relative artifact paths and expected SHA-256 digests
- `signature`: `{ "alg": "ES256", "value": "..." }`

The signature covers a deterministic canonical payload derived from the manifest identity, capability list, and artifact digests. The artifact files are read from the manifest directory and must stay inside that directory; absolute paths and `..` traversal are rejected.

Validate before trusting a package:

```sh
ardur marketplace validate ./manifest.json --key ./publisher-public-key.pem
```

Install local manifests or remote/package references:

```sh
ardur marketplace install ./manifest.json
ardur marketplace list
ardur marketplace verify --key ./publisher-public-key.pem
```

`verify --key` re-validates installed local manifest sources. Records without signatures are reported as untrusted.
