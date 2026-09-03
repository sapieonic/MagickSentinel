#!/usr/bin/env python3
"""Print sentinel-pipeline's runtime dependencies as a pip requirements list.

WHY THIS IS NOT JUST `pip install server/pipeline`.

The pipeline resolves its JSON Schemas by walking up from the module file:

    Path(__file__).resolve().parents[4] / "contracts" / "schemas" / "analysis.json"

That appears in `analysis/analyzer.py`, `compliance/judge.py`, `compliance/engine.py`
and `providers/anthropic.py`. Counted from
`server/pipeline/sentinel_pipeline/analysis/analyzer.py`, `parents[4]` is the
repository root — so the code depends on the package sitting exactly four directories
below a root that also contains `contracts/`.

`pip install .` copies the package into site-packages, where `parents[4]` is somewhere
inside the Python installation. The container would then start, pass an import check,
and raise `FileNotFoundError` on the first call it tried to analyse. So the image keeps
the source tree in place at `/app/server/pipeline` with `/app/contracts` beside it and
puts it on `PYTHONPATH` instead of installing it — which means the *dependencies* still
have to be installed from somewhere, and this script is that somewhere.

The alternative, hand-copying the dependency list into the Dockerfile, guarantees the
two drift the first time someone adds a package to pyproject.toml.

Extras are selected by name because they are genuinely optional and the choice is a
deployment decision with a residency dimension: provider SDKs are imported inside their
adapters (`# noqa: PLC0415`) so a Sarvam-only floor need not install Anthropic's client.
An image whose extras do not include the configured `SENTINEL_ASR_PROVIDER` fails on its
first call rather than at start, which is why the entrypoint validates the provider
selection before anything else.

Usage:
    pipeline-requirements.py <path-to-pyproject.toml> [extra[,extra...]]
"""

from __future__ import annotations

import pathlib
import sys
import tomllib  # Python 3.11+ standard library, so this script has no dependencies.


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2

    pyproject = pathlib.Path(argv[1])
    wanted = [e.strip() for e in (argv[2] if len(argv) > 2 else "").split(",") if e.strip()]

    data = tomllib.loads(pyproject.read_text(encoding="utf-8"))
    project = data.get("project", {})

    requirements: list[str] = list(project.get("dependencies", []))

    optional = project.get("optional-dependencies", {})
    for extra in wanted:
        if extra not in optional:
            # Fail rather than skip. A typo in the extras list would otherwise produce
            # an image that is missing exactly the provider SDK it was built to carry,
            # and the symptom appears on the first real call rather than in the build.
            print(
                f"error: extra {extra!r} is not declared in {pyproject}. "
                f"Declared extras: {', '.join(sorted(optional)) or '(none)'}",
                file=sys.stderr,
            )
            return 1
        requirements.extend(optional[extra])

    # Deduplicate while preserving order, so the printed list reads in the same order
    # as pyproject and a diff between two builds is legible. pip handles duplicates
    # fine; a human comparing two requirement lists does not.
    seen: set[str] = set()
    for req in requirements:
        if req not in seen:
            seen.add(req)
            print(req)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
