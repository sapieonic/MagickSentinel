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
# That makes a plain `pip install server/pipeline` WRONG in a way that does not fail at
# build time. Copied into site-packages, `parents[4]` points somewhere inside the Python
# installation; the image builds, the container starts, imports succeed — and the first
# call that reaches the analyser or the judge dies on a missing schema. The analyser
# validates its output against analysis.json and DISCARDS anything that does not match,
# so a missing schema file is not a degradation, it is a total stop for the paid half of
# the pipeline.
#
# The resolution, and it gets both halves:
#
#   * the source tree is copied verbatim to /app/server/pipeline, with `contracts/`
#     beside it at /app/contracts, so `parents[4]` resolves correctly;
#   * the package is installed EDITABLE (`pip install --no-deps -e`), which leaves the
#     source where it is and still produces the `sentinel-pipeline` console script that
#     pyproject.toml declares and that this image's ENTRYPOINT calls.
#
# `--no-deps` on that install because the dependencies were resolved and installed in
# the builder stage from the list deploy/containers/pipeline-requirements.py extracts
# out of pyproject.toml. Extracting them rather than hand-copying them into this file is
# what stops the two from drifting the first time someone adds a package.
#
# A layout check at the end of the build asserts the `parents[4]` relationship holds,
# because it is exactly the kind of thing a well-meaning "just pip install it" refactor
# breaks silently.
#
# ---------------------------------------------------------------------------------
# OTHER DESIGN NOTES
#
# **Why `-slim` and not distroless.** The gateway is one static binary and distroless
# fits it perfectly. This is CPython with C extension wheels (psycopg's binary build,
# pydantic-core) and, depending on extras, more of them. distroless/python pins its own
# interpreter version and has no pip, so every wheel would have to be staged by hand for
# no security gain over a slim image with no build toolchain. The hardening that matters
# here is: non-root, no compiler in the final stage, and a layout that works with a
# read-only root filesystem.
#
# **Why a builder stage at all, then.** psycopg[binary] ships wheels, but a platform
# without one falls back to compiling, which needs gcc and libpq headers. Keeping that
# in a stage that is thrown away means the runtime image never contains a compiler.
#
# **Extras are a build argument with a residency dimension.** Provider SDKs are optional
# extras, imported inside their adapters so a Sarvam-only floor need not install
# Anthropic's client. The default here is `google,opus`, matching
# `providers/registry.py`'s default batch provider and the Opus decoder the consumer
# needs to read stored segments.
#
# Note what the `google` default means for OPEN-4: the Gemini API is reached through a
# global endpoint, so the default sends borrower audio out of India. That is *processing*
# rather than storage, and it is the default rather than something someone opted into.
# If the bank's answer is India-only in the strict sense, build with
# `--build-arg PIPELINE_EXTRAS=sarvam,opus` and set SENTINEL_ASR_PROVIDER=sarvam. Both
# are configuration rather than code, which is why the registry exists.
#
# **No otlp-grpc extra.** `sentinel_pipeline/telemetry.py` defaults to `http/protobuf`
# and the HTTP exporter is a base dependency; the collector accepts both on 4318 and
# 4317 respectively. Adding the gRPC extra would mean carrying grpcio — a large wheel
# with its own native build — for a protocol nothing here asks for.
# ---------------------------------------------------------------------------------

ARG PYTHON_VERSION=3.12
# Extras from server/pipeline/pyproject.toml: sarvam | anthropic | openai | google |
# whisper | opus | otlp-grpc. Comma-separated. Must include the SDK for the configured
# SENTINEL_ASR_PROVIDER, or the container passes `sentinel-pipeline check` and then
# fails on its first real call.
ARG PIPELINE_EXTRAS=google,opus

# ================================================================== build =========
FROM python:${PYTHON_VERSION}-slim AS build

ARG PIPELINE_EXTRAS

ENV PIP_DISABLE_PIP_VERSION_CHECK=1 \
    PIP_NO_CACHE_DIR=1 \
    PYTHONDONTWRITEBYTECODE=1

# Build dependencies for the C extension wheels that may not have a prebuilt wheel for
# this platform. `libopus-dev` because the `opus` extra's opuslib binds libopus, and
# `libpq-dev` for psycopg's source fallback. Present only in this stage.
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential libpq-dev libopus-dev \
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
# self-contained directory, and `pip list` in the runtime image then describes the
# application rather than the base image.
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

# libopus at runtime (the -dev headers stay in the builder), and libpq for psycopg if it
# fell back to a source build. Both are small; neither brings a compiler.
RUN apt-get update \
 && apt-get install -y --no-install-recommends libopus0 libpq5 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /opt/venv /opt/venv

# ---- the layout the schema resolution depends on. Do not flatten this. ----
WORKDIR /app
COPY server/pipeline/ /app/server/pipeline/
# contracts/ is the source of truth for the analysis, judge and rule-set schemas, and
# the pipeline validates against them at runtime. Copied in rather than mounted so the
# image is self-contained and the schemas cannot drift from the code they were tested
# with.
COPY contracts/ /app/contracts/

# Editable, and --no-deps. Editable leaves the source at /app/server/pipeline so
# parents[4] still resolves to /app; --no-deps because the builder stage already
# installed everything from the extracted requirements, and letting pip re-resolve here
# would silently pull whatever is newest today rather than what was built and tested.
RUN pip install --no-deps --no-build-isolation -e /app/server/pipeline \
 && sentinel-pipeline --help > /dev/null \
 && echo "console script installed"

# Assert the schema relationship rather than trusting it. If a future edit installs the
# package non-editable or moves the tree, this fails at build time instead of on the
# first call a customer's floor sends through the analyser.
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
        f"schema layout is wrong: parents[4] resolved to {root}, and {schemas} is "
        f"missing {missing}. The pipeline package must stay at /app/server/pipeline "
        "with /app/contracts beside it -- see the header of "
        "deploy/containers/pipeline.Dockerfile. Do NOT fix this by pip-installing the "
        "package non-editable; that is what breaks it."
    )
print(f"schema layout ok: root={root}")
PY

# ---- non-root ----
# A fixed uid/gid, created explicitly rather than relying on a base-image user, so a
# Kubernetes securityContext and a compose `user:` can name the same number and a
# mounted volume's ownership is predictable.
RUN groupadd --system --gid 65532 sentinel \
 && useradd --system --uid 65532 --gid 65532 --home-dir /app --shell /usr/sbin/nologin sentinel \
 && chown -R 65532:65532 /app
USER 65532:65532

# `sentinel-pipeline check` is the repository's own configuration validator: it builds
# everything the consumer builds, opens the database, asserts row-level security is
# actually being enforced for the connecting role, and exits. Its docstring says why it
# exists — every startup failure this service can have is one somebody would rather find
# in a deploy step than in a stream of failed calls.
#
# BE CLEAR ABOUT WHAT THIS PROBE IS. It is a DEPENDENCY check, not a liveness check for
# the consume loop. It proves the database, NATS, the object store and the ASR selection
# are all reachable and correct; it cannot prove the JetStream loop is still turning,
# because the consumer exposes no liveness surface. A wedged consumer will pass this.
#
# That is a real gap and it is named rather than papered over. Closing it properly means
# an HTTP or file liveness surface in server/pipeline, which this work stream does not
# own. In the meantime the thing that actually catches a stalled consumer is the
# SentinelIngestStopped / SentinelPipelineTelemetryAbsent alerting in
# deploy/observability/rules/sentinel.rules.yml — which is where a stall should be
# caught anyway, because a stalled consumer on one replica of three is not a container
# health problem.
#
# 60s, not 15s: each run opens a database connection and a NATS connection. At 15s
# across a scaled worker pool that is a meaningful load for a check whose answer changes
# rarely.
HEALTHCHECK --interval=60s --timeout=20s --start-period=45s --retries=3 \
    CMD ["sentinel-pipeline", "check"]

ENTRYPOINT ["sentinel-pipeline"]
# `consume` is the long-running mode. `retention` and `coverage` are the two scheduled
# jobs and are run as one-shot containers with the same image — see the `retention` and
# `coverage` services in deploy/compose.yaml, both in the `tools` profile.
CMD ["consume"]
