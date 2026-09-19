# Release artifacts and verification

The [release workflow](../.github/workflows/release.yml) builds six binaries
for each of these native, public GitHub-hosted runner targets:

| Target | Runner | Binary format |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` | ELF64 x86-64 |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | ELF64 AArch64 |
| `x86_64-apple-darwin` | `macos-15-intel` | thin Mach-O x86_64 |
| `aarch64-apple-darwin` | `macos-15` | thin Mach-O arm64 |

Runner labels were checked against the [GitHub-hosted runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
Linux ARM64 builds natively; this workflow does not use private runners,
cross-compilation, or universal macOS binaries. Every build uses Rust 1.98.1
and `--locked`, validates binary architecture, and extracts and executes the
packaged `ardur --version` on its matching runner.

The binaries are `ardur`, `ardur-server`, `ardur-admin`, `ardur-eval`,
`ardur-healthcheck`, and `ardur-memory-eval`. Each archive is named
`<binary>-<tag>-<target>.tar.gz` and contains that executable at its root.
Intel macOS archives also contain `licenses/onnxruntime/` notices.
Linux GNU binaries require a compatible glibc-based system; these are not
static musl builds.

## Release-level supply-chain contract

Publishing a GitHub release triggers the workflow; pushing a tag alone does
not. Older releases, including v0.2.0, have only Linux x86-64 archives. Check
the assets and successful workflow for the particular release you select;
a source tag or a green PR is not proof that release assets exist.

All four lanes must pass before the single release signing job runs. It
requires exactly 24 archives, generates one release-level SPDX SBOM from the
checked-out workspace (the existing granularity, not a per-binary SBOM), and
writes one `SHA256SUMS` covering all 24 archives and that SBOM. Each of those
25 payloads, plus `SHA256SUMS`, gets a keyless Sigstore `.bundle` and GitHub
build-provenance attestation. The checksum file does not recursively hash
itself or the signature bundles. Cosign verifies every payload and the
checksum file, and the complete inventory and checksums must pass before
release upload. A missing lane, binary, bundle, or checksum entry fails the
release job rather than publishing a reduced set.

The SBOM action's automatic release upload is disabled so it cannot publish
an unsigned SBOM ahead of this gate. The final upload remains a GitHub API
operation, not a transactional release: if an upload fails, treat the release
as incomplete until a successful rerun and full asset verification.

### Intel macOS native dependency

The locked `ort-sys` crate provides no Intel macOS prebuilt distribution.
This lane therefore uses the static ONNX Runtime 1.28.0 library from
[`csukuangfj/onnxruntime-libs`](https://github.com/csukuangfj/onnxruntime-libs/releases/tag/v1.28.0),
an additional binary supplier explicitly [approved for P0](https://github.com/ArdurAI/ardur-agent/issues/532#issuecomment-5739593965).
The archive SHA256 is
`88c6037c0eb9a7f0013729e0181feec4fb9ef30ca37769aa7b69d2478450c415`.
The workflow checks this before extraction, verifies the library's x86_64
architecture, and selects static linking through `ORT_LIB_PATH`. No Rust
feature is disabled and no external ONNX dylib is required. The release-level
SBOM explicitly records this supplier, version, URL, and input digest; this
record is not a claim of upstream build-provenance verification. License and
third-party notices are fetched from the pinned Microsoft source commit with
separate checksums and included in every Intel archive. Other lanes retain
the locked crate's existing native dependency download path.

## Download and verify before executing

Requires GitHub CLI and Cosign v3. Use a new empty directory. Set `tag` to a
published release with the four-target matrix; select the exact target from
the table. The example names the P0 verification prerelease, not `latest`.

```sh
set -eu
tag=v0.2.1-rc.1
target=aarch64-apple-darwin
artifact="ardur-${tag}-${target}.tar.gz"
repo=ArdurAI/ardur-agent
identity="https://github.com/${repo}/.github/workflows/release.yml@refs/tags/${tag}"
issuer=https://token.actions.githubusercontent.com

# Download only the selected archive and the checksum trust material.
gh release download "$tag" --repo "$repo" \
  --pattern "$artifact" --pattern "${artifact}.bundle" \
  --pattern SHA256SUMS --pattern SHA256SUMS.bundle

cosign verify-blob --bundle SHA256SUMS.bundle \
  --certificate-identity "$identity" --certificate-oidc-issuer "$issuer" \
  SHA256SUMS
cosign verify-blob --bundle "${artifact}.bundle" \
  --certificate-identity "$identity" --certificate-oidc-issuer "$issuer" \
  "$artifact"

# Require exactly one checksum for this archive; do not accept a missing row.
python3 - "$artifact" <<'PY'
import pathlib, sys
name = sys.argv[1]
rows = [row for row in pathlib.Path("SHA256SUMS").read_text().splitlines()
        if row.split("  ", 1)[-1].removeprefix("./") == name]
if len(rows) != 1:
    raise SystemExit("expected exactly one checksum entry for " + name)
pathlib.Path("SHA256SUMS.selected").write_text(rows[0] + "\n")
PY

# Linux (or macOS with GNU coreutils):
sha256sum --check SHA256SUMS.selected
# On stock macOS, use this INSTEAD of the preceding command:
# shasum -a 256 --check SHA256SUMS.selected

tar -xzf "$artifact"
./ardur --version
```

For the entire release, download all assets and run `sha256sum --check
SHA256SUMS` (or `shasum -a 256 --check SHA256SUMS` on macOS), then run the
same `cosign verify-blob` command on every `.tar.gz`, `.spdx.json`, and
`SHA256SUMS` with its corresponding bundle. Keep the exact tag-specific
certificate identity and GitHub OIDC issuer checks; do not replace them with
an unrestricted identity regex. To verify GitHub provenance additionally:

```sh
gh attestation verify "$artifact" --repo ArdurAI/ardur-agent
```

Keep the release URL, exact source SHA, workflow run URL, individual lane
job URLs, smoke output, and downloaded-file checksum/Cosign exit-zero
outputs with release evidence. Header fixtures in unit tests are not real
release-build or cryptographic evidence.

## macOS Gatekeeper and notarization

For P0, the [owner approved distribution without Apple Developer ID signing
or notarization](https://github.com/ArdurAI/ardur-agent/issues/532#issuecomment-5739491622).
This is separate from the mandatory Sigstore signatures above. No Apple
identity or credentials are assumed. Notarization implementation is deferred.

A browser-downloaded archive or extracted executable can carry Apple's
quarantine attribute. Only after verifying the archive as above, if Gatekeeper
blocks the extracted executable, remove that attribute from that file:

```sh
xattr -d com.apple.quarantine ./ardur
./ardur --version
```

If the attribute is absent, no removal is needed. Do not disable Gatekeeper
system-wide or remove quarantine recursively from unrelated files. Homebrew,
npm, and binstall distribution are separate milestones in [#532](https://github.com/ArdurAI/ardur-agent/issues/532),
not channels delivered by this workflow change.
