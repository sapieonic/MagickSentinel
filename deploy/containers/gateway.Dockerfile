# syntax=docker/dockerfile:1.7
#
# server/gateway — the Go REST API and WSS ingest endpoint.
#
# BUILD CONTEXT IS THE REPOSITORY ROOT, not server/gateway. That is deliberate and it
# is not just about the health probe: it is how this file can build from
# server/gateway without editing anything inside it, which is the constraint this
# work stream is under. Build it as:
#
#   docker build -f deploy/containers/gateway.Dockerfile -t sentinel-gateway .
#
# ---------------------------------------------------------------------------------
# DESIGN NOTES, in the order someone reading this will wonder about them.
#
# **Multi-stage, and the final stage is distroless static.** The gateway is a single
# statically linked binary with CGO off, so the runtime needs no libc, no shell and no
# package manager. `gcr.io/distroless/static-debian12:nonroot` is about 2 MB and
# contains CA certificates, /etc/passwd with a nonroot user, and tzdata. Nothing else.
# That matters here more than it does for a typical service: this process holds the
# database credential for `sentinel_app`, terminates client-certificate mTLS, and is
# reachable from every agent desktop on a collections floor. An RCE primitive in a
# distroless image has nothing to exec.
#
# **CA certificates are load-bearing, not decoration.** The gateway verifies Identity
# Platform ID tokens against Google's JWKS over HTTPS
# (server/gateway/internal/auth/auth.go). A FROM scratch image would have no root
# store, `CachingKeySource` would fail every fetch, and the gateway would answer 401
# to every authenticated request while /healthz stayed green — the product's
# characteristic failure shape, reached through a Dockerfile.
#
# **Non-root, and the number is fixed at 65532.** distroless:nonroot is uid 65532. It
# is stated explicitly with USER rather than left to the tag so that a `-debug` tag
# swap for troubleshooting cannot silently promote the process to root, and so a
# Kubernetes runAsUser can be matched to it.
#
# **No VOLUME for object storage.** `main.go` requires either SENTINEL_S3_BUCKET (the
# real backend) or SENTINEL_BLOB_DIR (the filesystem stand-in) and refuses to start with
# neither. Whoever runs this image has to choose knowingly. A VOLUME here would produce
# an anonymous volume that looks like durable storage, holds borrower call audio, and is
# discarded by `docker compose down -v` — and OPEN-4 (India-only residency) is
# unresolved, so an anonymous volume is also a storage location nobody has approved.
#
# **No HTTPS in the image's own healthcheck.** The gateway serves plaintext unless
# SENTINEL_TLS_CERT is set, and the probe talks to 127.0.0.1 inside the container's
# own network namespace, where there is no transport to protect. When you run this
# behind TLS, the loopback listener is still the same listener.
# ---------------------------------------------------------------------------------

ARG GO_VERSION=1.25
ARG RUNTIME_IMAGE=gcr.io/distroless/static-debian12:nonroot

# ================================================================== build =========
FROM golang:${GO_VERSION}-bookworm AS build

# Version stamped into the binary and reported by /healthz and every heartbeat
# response. Passed in rather than derived, because the build context has no .git when
# built by CI from a checkout with `.dockerignore` in play, and a binary that reports
# "dev" in production is a binary nobody can trace back to a commit.
ARG VERSION=dev

WORKDIR /src

# Dependencies first, in their own layer, so a change to a .go file does not
# re-download the module graph. go.sum is copied with go.mod so `go mod download`
# verifies rather than resolves.
COPY server/gateway/go.mod server/gateway/go.sum ./gateway/
RUN --mount=type=cache,target=/go/pkg/mod \
    cd gateway && go mod download

COPY server/gateway/ ./gateway/
COPY deploy/containers/healthprobe/ ./healthprobe/

# CGO_ENABLED=0 is what makes the binary runnable on a distroless static base. It
# also changes DNS resolution to Go's pure-Go resolver, which is the behaviour we
# want in a container: the cgo resolver reads /etc/nsswitch.conf, which distroless
# does not have.
#
# -trimpath keeps the builder's absolute paths out of the binary, so two builds of the
# same commit produce the same bytes and panic traces do not leak a build layout.
# -s -w drop the symbol table and DWARF: the gateway's panics are recovered and logged
# with a request ID by httpx.Recover, so a stack with function names is enough and a
# full DWARF section is 30% of the binary for no operational benefit.
ENV CGO_ENABLED=0 GOOS=linux
RUN --mount=type=cache,target=/go/pkg/mod \
    --mount=type=cache,target=/root/.cache/go-build \
    cd gateway && \
    go build -trimpath -ldflags "-s -w -X main.version=${VERSION}" -o /out/gateway ./cmd/gateway

RUN --mount=type=cache,target=/go/pkg/mod \
    --mount=type=cache,target=/root/.cache/go-build \
    cd healthprobe && \
    go build -trimpath -ldflags "-s -w" -o /out/healthprobe .

# ================================================================ runtime =========
FROM ${RUNTIME_IMAGE} AS runtime

ARG VERSION=dev

# OCI labels, so `docker inspect` on a running container in a customer's estate can
# answer "what is this and which commit is it" without asking us.
LABEL org.opencontainers.image.title="MagickVoice Sentinel gateway" \
      org.opencontainers.image.description="REST API and WSS ingest for Sentinel call monitoring." \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.source="https://github.com/magickvoice/sentinel" \
      org.opencontainers.image.licenses="Proprietary" \
      org.opencontainers.image.base.name="gcr.io/distroless/static-debian12:nonroot"

COPY --from=build /out/gateway /usr/local/bin/gateway
COPY --from=build /out/healthprobe /usr/local/bin/healthprobe

# uid 65532, distroless's `nonroot`. Stated numerically as well as by name so that a
# Kubernetes securityContext (runAsNonRoot: true, runAsUser: 65532) can be written
# against it without inspecting the image.
USER 65532:65532

# Matches main.go's `envOr("SENTINEL_ADDR", ":8080")`. Documentation only — EXPOSE
# publishes nothing — but it is what `docker ps` shows and what a reader checks first.
EXPOSE 8080

# BOTH /healthz and /readyz are required, and requiring /readyz is the deliberate part.
#
# /healthz answers 200 as long as the process is alive. /readyz runs the dependency
# probes — database, object store — and answers 503 when one is down. The gateway's own
# comment on readyz explains why that difference matters here: a gateway that is up and
# cannot reach its object store answers the ingest WebSocket, takes the audio, fails the
# blob write and loses the call, silently from the desktop's point of view, because the
# segment goes unacked and sits in the spool until the 72-hour bound evicts it.
#
# So a container in that state must report unhealthy. Probing only liveness would report
# a container that is destroying audio as healthy — which is this product's
# characteristic failure mode reached through a Dockerfile.
#
# start-period is 30s: `store.Open` establishes the pgx pool before the HTTP listener
# starts, and the readiness probes then have to reach Postgres and the object store. A
# gateway pointed at a Postgres still running initdb is legitimately not ready yet, and
# failing it during that window would make compose report a dependency-order problem as
# an application fault.
HEALTHCHECK --interval=15s --timeout=8s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/healthprobe", \
         "-base", "http://127.0.0.1:8080", \
         "-required", "/healthz,/readyz"]

ENTRYPOINT ["/usr/local/bin/gateway"]
