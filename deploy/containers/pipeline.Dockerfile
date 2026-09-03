# syntax=docker/dockerfile:1.7
#
# server/pipeline — the ASR, analysis and RBI compliance workers.
#
# BUILD CONTEXT IS THE REPOSITORY ROOT. Build it as:
#
#   docker build -f deploy/containers/pipeline.Dockerfile -t sentinel-pipeline .
#
# ---------------------------------------------------------------------------------
# THE ONE THING TO UNDERSTAND BEFORE CHANGING ANYTHING HERE
#
# The pipeline finds its JSON Schemas by walking up from its own module file:
#
#     Path(__file__).resolve().parents[4] / "contracts" / "schemas" / "analysis.json"
#
# — in `analysis/analyzer.py`, `compliance/judge.py`, `compliance/engine.py` and
# `providers/anthropic.py`. From
# `server/pipeline/sentinel_pipeline/analysis/analyzer.py`, `parents[4]` is the
# repository root. So the package must sit exactly four directories below a directory
# that also contains `contracts/`.
#
# That makes `pip install server/pipeline` WRONG in a way that does not fail at build
# time. Installed into site-packages, `parents[4]` points somewhere inside the Python
# installation, the image builds, the container starts, imports succeed — and the first
# call that reaches the analyser or the judge raises FileNotFoundError. So:
#
#   * the source tree is copied verbatim to /app/server/pipeline;
#   * `contracts/` is copied to /app/contracts, beside it;
#   * PYTHONPATH=/app/server/pipeline puts the package on the path WITHOUT relocating
#     it;
#   * dependencies are installed separately, from the list
#     deploy/containers/pipeline-requirements.py extracts out of pyproject.toml, so
#     that list cannot drift from the manifest.
#
# A layout check at the end of the build asserts the relationship holds, because it is
# the kind of thing a well-meaning "just pip install it" refactor breaks silently.
#
# ---------------------------------------------------------------------------------
# OTHER DESIGN NOTES
#
# **Why `-slim` and not distroless.** The gateway is one static binary and distroless
# fits it perfectly. This is CPython with C extension wheels (asyncpg, pydantic-core)
# and, depending on extras, more of them. distroless/python exists but pins its own
# interpreter version and offers no pip, so every wheel would have to be staged by
# hand for no security gain over a slim image with no compilers and no shell tools
# beyond what CPython itself needs. `python:3.12-slim` is the honest choice; the
# hardening that matters here is non-root, no build toolchain in the final stage, and a
# read-only-friendly layout.
#
# **Why a separate build stage at all, then.** The wheels for asyncpg and pydantic-core
# may compile from source on a platform without a prebuilt wheel, which needs gcc.
# Keeping that in a builder stage means the runtime image never contains a compiler —
# which is both a smaller image and one fewer thing for a security review to ask about.
#
# **Extras are a build argument with a residency dimension.** Provider SDKs are
# optional extras, imported inside their adapters so a Sarvam-only floor need not
# install Anthropic's client. The default here is `google`, matching
# `providers/registry.py`'s `DEFAULT_BATCH_ASR`. Note what that default means: the
# Gemini API endpoint is global, so it is borrower audio leaving India — OPEN-4's
# residency question is open and the ASR default has taken a position on it. If the
# bank's answer is India-only in the strict sense, this image should be built with
# `--build-arg PIPELINE_EXTRAS=sarvam` and run with SENTINEL_ASR_PROVIDER=sarvam. That
# is configuration, not code, which is why the registry exists.
#
# **The healthcheck is a liveness file, not an endpoint.** The pipeline is a JetStream
# consumer with no HTTP surface. Adding one would mean editing server/pipeline, which
# this work stream does not own. See the long note in pipeline-entrypoint.py.
# ---------------------------------------------------------------------------------

ARG PYTHON_VERSION=3.12
# Extras from server/pipeline/pyproject.toml: sarvam | anthropic | openai | google |
# whisper. Comma-separated. Must include the SDK for the configured
# SENTINEL_ASR_PROVIDER, or the container starts and then fails on its first call.
ARG PIPELINE_EXTRAS=google

# ================================================================== build =========
FROM python:${PYTHON_VERSION}-slim AS build

ARG PIPELINE_EXTRAS

ENV PIP_DISABLE_PIP_VERSION_CHECK=1 \
    PIP_NO_CACHE_DIR=1 \
    PYTHONDONTWRITEBYTECODE=1

# Build dependencies for the C extension wheels that may not have a prebuilt wheel for
# this platform (asyncpg, pydantic-core). Present only in this stage.
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Only the manifest and the extractor, so the dependency layer is invalidated by a
# pyproject change and not by a change to a Python module.
COPY server/pipeline/pyproject.toml ./pyproject.toml
COPY deploy/containers/pipeline-requirements.py ./pipeline-requirements.py

RUN python pipeline-requirements.py ./pyproject.toml "${PIPELINE_EXTRAS}" > /build/requirements.txt \
 && echo "--- resolved runtime dependencies (extras: ${PIPELINE_EXTRAS}) ---" \
 && cat /build/requirements.txt

# A venv rather than the system site-packages: it copies to the runtime stage as one
# self-contained directory, and it keeps the image's Python packages separate from
# whatever the base image itself installed, so `pip list` in the runtime image
# describes the application and nothing else.
RUN python -m venv /opt/venv \
 && /opt/venv/bin/pip install --upgrade pip setuptools wheel \
 && /opt/venv/bin/pip install -r /build/requirements.txt

# ================================================================ runtime =========
FROM python:${PYTHON_VERSION}-slim AS runtime

ARG PIPELINE_EXTRAS

LABEL org.opencontainers.image.title="MagickVoice Sentinel pipeline" \
      org.opencontainers.image.description="ASR, analysis and RBI fair-practices compliance workers." \
      org.opencontainers.image.source="https://github.com/magickvoice/sentinel" \
      org.opencontainers.image.licenses="Proprietary" \
      com.magickvoice.sentinel.asr-extras="${PIPELINE_EXTRAS}"

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PATH="/opt/venv/bin:${PATH}"

COPY --from=build /opt/venv /opt/venv

# ---- the layout the schema resolution depends on. Do not flatten this. ----
WORKDIR /app
COPY server/pipeline/ /app/server/pipeline/
# contracts/ is the source of truth for the analysis, judge and rule-set schemas, and
# the pipeline validates against them at runtime — an analyser output that does not
# match analysis.json is discarded rather than stored, so a missing schema file is not
# a degradation, it is a total stop. Copied in, not mounted, so the image is
# self-contained and the schemas cannot drift from the code that was tested with them.
COPY contracts/ /app/contracts/
COPY deploy/containers/pipeline-entrypoint.py /app/entrypoint.py

# PYTHONPATH, not an install. This is the whole point of the layout above.
ENV PYTHONPATH=/app/server/pipeline

# Assert the relationship rather than trusting it. `parents[4]` from the analyser
# module must be a directory containing contracts/schemas — if a future edit installs
# the package or moves the tree, this fails at build time instead of on the first call
# a customer's floor sends through the analyser.
RUN python - <<'PY'
import pathlib
import sys

import sentinel_pipeline.analysis.analyzer as analyzer

root = pathlib.Path(analyzer.__file__).resolve().parents[4]
schemas = root / "contracts" / "schemas"
required = ["analysis.json", "judge.json", "rule_set.json"]
missing = [name for name in required if not (schemas / name).is_file()]
if missing:
    sys.exit(
        f"schema layout is wrong: parents[4] resolved to {root}, and "
        f"{schemas} is missing {missing}. The pipeline package must stay at "
        "/app/server/pipeline with /app/contracts beside it -- see the header of "
        "deploy/containers/pipeline.Dockerfile. Do NOT fix this by pip-installing "
        "the package; that is what breaks it."
    )
print(f"schema layout ok: root={root}")
PY

# ---- non-root ----
# A fixed uid/gid, created explicitly rather than relying on a base-image user, so a
# Kubernetes securityContext and a compose `user:` can both name the same number and a
# mounted volume's ownership is predictable.
RUN groupadd --system --gid 65532 sentinel \
 && useradd --system --uid 65532 --gid 65532 --home-dir /app --shell /usr/sbin/nologin sentinel \
 && chown -R 65532:65532 /app
USER 65532:65532

# The liveness file lives in /tmp so the rest of the filesystem can be mounted
# read-only (`read_only: true` in compose, `readOnlyRootFilesystem` in Kubernetes) with
# only a tmpfs for /tmp. A liveness file under /app would make a read-only root
# impossible, which is a real hardening loss for the sake of one path.
ENV SENTINEL_LIVENESS_FILE=/tmp/pipeline-alive

# `python -c` rather than a shell test, because the check is "the loop refreshed this
# within the last 45 seconds", not "the file exists". A file that exists but has not
# been touched since start is precisely the failure a liveness probe is for: the
# process is alive and the loop has stopped.
#
# 45s against the entrypoint's 10s refresh gives four missed refreshes of slack, which
# is enough to ride out a long analysis in consume mode without being so generous that
# a wedged loop stays green for minutes.
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 \
    CMD ["python", "-c", "import os,sys,time;p=os.environ['SENTINEL_LIVENESS_FILE'];sys.exit(0 if os.path.exists(p) and time.time()-os.path.getmtime(p) < 45 else 1)"]

ENTRYPOINT ["python", "/app/entrypoint.py"]
