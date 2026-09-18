"""Guards for the SPIFFE/SPIRE honesty claim (gh#365).

`SECURITY.md` states that SPIFFE-style identifiers are a naming convention with
no SPIRE integration behind them. That claim is only trustworthy while it stays
true, and documentation drifts silently: someone adds a SPIFFE dependency and
the prose quietly becomes a lie, or someone rewrites the prose and the caveat
disappears while the gap remains.

These tests pin both directions.
"""

import re
import subprocess
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
SECURITY = REPO / "SECURITY.md"

# Word-boundary matching: "inspired" contains "spire" and is not a SPIFFE claim.
SPIFFE_WORD = re.compile(r"\bspiffe\b|\bspire\b", re.IGNORECASE)


def tracked_files(*suffixes):
    """Files git actually tracks, so untracked local scratch cannot fail CI."""
    out = subprocess.run(
        ["git", "ls-files"], cwd=REPO, capture_output=True, text=True, check=True
    ).stdout.splitlines()
    return [REPO / p for p in out if p.endswith(suffixes)]


class SpiffeHonestyTests(unittest.TestCase):
    def test_security_md_documents_the_attestation_gap(self):
        """The caveat must survive edits to SECURITY.md.

        Without this, a tidy-up that drops the section leaves the repo silently
        back where gh#365 started: SPIFFE-shaped names with nothing behind them
        and no statement saying so.
        """
        text = SECURITY.read_text(encoding="utf-8")
        for phrase in (
            "naming convention, not an",
            "operator-scoped keypair",
            "attesting itself",
        ):
            self.assertIn(
                phrase.lower(),
                text.lower(),
                f"SECURITY.md no longer explains the identity gap: {phrase!r} is missing",
            )

    def test_no_spiffe_dependency_exists(self):
        """The honesty claim is false the moment a real SPIFFE crate appears.

        This test is expected to FAIL when the gh#514 integration lands — that
        failure is the reminder to rewrite SECURITY.md rather than ship prose
        that understates what the code now does.
        """
        offenders = []
        for manifest in tracked_files("Cargo.toml"):
            for i, line in enumerate(
                manifest.read_text(encoding="utf-8").splitlines(), start=1
            ):
                stripped = line.strip()
                if stripped.startswith("#"):
                    continue
                if SPIFFE_WORD.search(stripped):
                    offenders.append(f"{manifest.relative_to(REPO)}:{i}: {stripped}")
        self.assertEqual(
            offenders,
            [],
            "a SPIFFE/SPIRE dependency now exists, so SECURITY.md's "
            "'no integration' claim is stale:\n" + "\n".join(offenders),
        )

    def test_docs_do_not_claim_bare_spiffe_identity(self):
        """Prose must qualify the naming, not assert integrated SPIFFE identity.

        Checks tracked markdown only, and requires a qualifier near each
        mention. `architect/` session archives are historical records of what
        was believed at the time and are excluded deliberately.
        """
        qualifiers = (
            "spiffe-style",
            "naming convention",
            "not an attested",
            "server-mode-only",
            "server/fleet",
            "no spiffe",
            "not an integration",
        )
        offenders = []
        for doc in tracked_files(".md"):
            rel = doc.relative_to(REPO).as_posix()
            if rel.startswith("architect/"):
                continue
            # SECURITY.md is the canonical explanation of the gap: it discusses
            # SPIFFE and SPIRE at length precisely to say what is NOT there, so
            # requiring a qualifier beside every mention there is nonsense. Its
            # content is pinned by the dedicated test above instead.
            if rel == "SECURITY.md":
                continue
            lines = doc.read_text(encoding="utf-8").splitlines()
            for i, line in enumerate(lines):
                if not SPIFFE_WORD.search(line):
                    continue
                # A qualifier may sit on the mention's line or the next one,
                # since prose wraps.
                window = " ".join(lines[i : i + 2]).lower()
                if not any(q in window for q in qualifiers):
                    offenders.append(f"{rel}:{i + 1}: {line.strip()}")
        self.assertEqual(
            offenders,
            [],
            "unqualified SPIFFE/SPIRE claims in tracked docs (say "
            "'SPIFFE-style' or point at SECURITY.md):\n" + "\n".join(offenders),
        )


if __name__ == "__main__":
    unittest.main()
