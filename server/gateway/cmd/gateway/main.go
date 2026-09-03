// Command gateway serves the Sentinel REST API and the WSS ingest endpoint.
package main

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"net/url"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/magickvoice/sentinel/server/gateway/internal/api"
	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
	"github.com/magickvoice/sentinel/server/gateway/internal/blob"
	"github.com/magickvoice/sentinel/server/gateway/internal/ca"
	"github.com/magickvoice/sentinel/server/gateway/internal/httpx"
	"github.com/magickvoice/sentinel/server/gateway/internal/idp"
	"github.com/magickvoice/sentinel/server/gateway/internal/ingest"
	"github.com/magickvoice/sentinel/server/gateway/internal/outbox"
	"github.com/magickvoice/sentinel/server/gateway/internal/store"
	"github.com/magickvoice/sentinel/server/gateway/internal/telemetry"
)

// version is set at build time with -ldflags "-X main.version=...".
var version = "dev"

type config struct {
	addr        string
	databaseURL string
	projectID   string
	environment string
	tlsCert     string
	tlsKey      string
	clientCAs   string

	// Object storage. S3 is preferred when a bucket is named; the filesystem
	// backend stays for development.
	blobDir string
	s3      blob.S3Config

	// The device certificate authority.
	ca ca.Config

	// The desktop's OAuth token endpoint.
	idp        idp.Config
	tokenRate  float64
	tokenBurst float64

	// Browser origins allowed to call the API cross-origin.
	allowedOrigins []string

	// NATS, and how hard the outbox drainer works.
	nats           outbox.NATSConfig
	outboxInterval time.Duration
	outboxBatch    int

	otel telemetry.Config
}

func loadConfig() (config, error) {
	c := config{
		addr:        envOr("SENTINEL_ADDR", ":8080"),
		databaseURL: os.Getenv("SENTINEL_DATABASE_URL"),
		projectID:   os.Getenv("SENTINEL_GCP_PROJECT"),
		environment: os.Getenv("SENTINEL_ENV"),
		tlsCert:     os.Getenv("SENTINEL_TLS_CERT"),
		tlsKey:      os.Getenv("SENTINEL_TLS_KEY"),
		clientCAs:   os.Getenv("SENTINEL_CLIENT_CA"),

		blobDir: os.Getenv("SENTINEL_BLOB_DIR"),
		s3: blob.S3Config{
			Bucket:          os.Getenv("SENTINEL_S3_BUCKET"),
			Region:          envOr("SENTINEL_S3_REGION", blob.DefaultRegion),
			Endpoint:        os.Getenv("SENTINEL_S3_ENDPOINT"),
			SSE:             os.Getenv("SENTINEL_S3_SSE"),
			KMSKeyID:        os.Getenv("SENTINEL_S3_KMS_KEY_ID"),
			AccessKeyID:     os.Getenv("SENTINEL_S3_ACCESS_KEY_ID"),
			SecretAccessKey: os.Getenv("SENTINEL_S3_SECRET_ACCESS_KEY"),
		},

		ca: ca.Config{
			CertPath:  os.Getenv("SENTINEL_CA_CERT"),
			KeyPath:   os.Getenv("SENTINEL_CA_KEY"),
			ChainPath: os.Getenv("SENTINEL_CA_CHAIN"),
		},

		idp: idp.Config{
			APIKey:         os.Getenv("SENTINEL_IDP_API_KEY"),
			ClientID:       os.Getenv("SENTINEL_OIDC_CLIENT_ID"),
			ClientSecret:   os.Getenv("SENTINEL_OIDC_CLIENT_SECRET"),
			TenantID:       os.Getenv("SENTINEL_IDP_TENANT"),
			OAuthTokenURL:  os.Getenv("SENTINEL_OIDC_TOKEN_URL"),
			SignInWithIdP:  os.Getenv("SENTINEL_IDP_SIGNIN_URL"),
			SecureTokenURL: os.Getenv("SENTINEL_IDP_SECURE_TOKEN_URL"),
			ProviderID:     os.Getenv("SENTINEL_OIDC_PROVIDER_ID"),
		},
		// Five sign-ins a second per address with a burst of ten. A desktop makes
		// one token request at sign-in and one every fifty minutes after that, so
		// this is three orders of magnitude above the real client and still tight
		// enough that the endpoint cannot be used to burn our IdP quota. A whole
		// floor behind one NAT is the case that makes the burst worth having.
		tokenRate:  envFloat("SENTINEL_TOKEN_RATE_PER_SEC", 5),
		tokenBurst: envFloat("SENTINEL_TOKEN_BURST", 10),

		allowedOrigins: splitList(os.Getenv("SENTINEL_ALLOWED_ORIGINS")),

		nats: outbox.NATSConfig{
			// SENTINEL_NATS_SERVERS is read as a fallback so one variable can
			// configure the gateway and the pipeline consumer, which names it
			// that way (server/pipeline/sentinel_pipeline/consumer.py).
			URL:           envOr("SENTINEL_NATS_URL", os.Getenv("SENTINEL_NATS_SERVERS")),
			CredsFile:     os.Getenv("SENTINEL_NATS_CREDS"),
			NKeySeedFile:  os.Getenv("SENTINEL_NATS_NKEY_SEED"),
			Token:         os.Getenv("SENTINEL_NATS_TOKEN"),
			User:          os.Getenv("SENTINEL_NATS_USER"),
			Password:      os.Getenv("SENTINEL_NATS_PASSWORD"),
			TLSCAFile:     os.Getenv("SENTINEL_NATS_CA"),
			TLSCertFile:   os.Getenv("SENTINEL_NATS_CLIENT_CERT"),
			TLSKeyFile:    os.Getenv("SENTINEL_NATS_CLIENT_KEY"),
			TLSHostname:   os.Getenv("SENTINEL_NATS_TLS_HOSTNAME"),
			AllowInsecure: envBool("SENTINEL_NATS_ALLOW_INSECURE"),
			Name:          "sentinel-gateway",
		},
		outboxInterval: envDuration("SENTINEL_OUTBOX_INTERVAL", 2*time.Second),
		outboxBatch:    int(envFloat("SENTINEL_OUTBOX_BATCH", 64)),

		otel: telemetry.Config{
			Enabled: envBool("SENTINEL_OTEL_ENABLED"),
			// The standard variable, so a collector sidecar injected by the
			// platform is picked up without a Sentinel-specific setting.
			Endpoint:    os.Getenv("OTEL_EXPORTER_OTLP_ENDPOINT"),
			Insecure:    envBool("SENTINEL_OTEL_INSECURE"),
			Version:     version,
			Environment: os.Getenv("SENTINEL_ENV"),
			SampleRatio: envFloat("SENTINEL_OTEL_SAMPLE_RATIO", 1),
		},
	}

	if c.databaseURL == "" {
		return c, errors.New("SENTINEL_DATABASE_URL is required")
	}
	if c.projectID == "" {
		return c, errors.New("SENTINEL_GCP_PROJECT is required to verify ID tokens")
	}
	// A certificate authority is required to start, not optional at runtime.
	//
	// The alternative — which is what this service did until now — is a gateway
	// that boots happily and answers 503 no_ca to every enrollment. That failure
	// surfaces at the worst possible moment: the first desktop of a floor rollout,
	// with an installer engineer on site, and it looks like a client problem. A
	// missing CA is a deployment mistake and belongs in the deploy's own logs.
	if c.ca.CertPath == "" || c.ca.KeyPath == "" {
		return c, errors.New("SENTINEL_CA_CERT and SENTINEL_CA_KEY are required: " +
			"without them POST /v1/devices/enroll answers 503 for every device")
	}
	if err := checkOrigins(c.allowedOrigins); err != nil {
		return c, err
	}
	return c, nil
}

// checkOrigins validates SENTINEL_ALLOWED_ORIGINS before httpx.CORS sees it.
//
// httpx.CORS panics on a wildcard, which is the correct behaviour for a programming
// error and the wrong one for a typo in an environment variable: a panic at the first
// request is harder to read than a refusal at startup naming the variable. So the
// same rule is applied here, with the reason spelled out.
//
// The reason: this API sends Access-Control-Allow-Credentials, because the portal
// authenticates with a bearer token. A wildcard origin combined with credentials
// would let any page on the internet read a tenant's borrower call summaries with a
// token it managed to obtain — and browsers reject that combination anyway, so a
// wildcard here does not even produce a working portal. It produces a portal that
// fails in a way nobody attributes to this setting.
//
// A path, a query or a trailing slash is also refused: the Origin header a browser
// sends is scheme + host + port and nothing else, so an entry with a path can never
// match and would silently disable CORS for the origin someone meant to allow.
func checkOrigins(origins []string) error {
	for _, o := range origins {
		if o == "*" {
			return errors.New("SENTINEL_ALLOWED_ORIGINS must not contain '*': " +
				"this API sends Access-Control-Allow-Credentials, and a wildcard " +
				"origin with credentials would expose tenant call content to any " +
				"page on the internet (browsers reject the combination in any case)")
		}
		u, err := url.Parse(o)
		if err != nil || u.Scheme == "" || u.Host == "" {
			return fmt.Errorf("SENTINEL_ALLOWED_ORIGINS: %q is not a scheme://host[:port] origin", o)
		}
		if u.Path != "" || u.RawQuery != "" || u.Fragment != "" {
			return fmt.Errorf("SENTINEL_ALLOWED_ORIGINS: %q must be scheme://host[:port] "+
				"with no path; a browser's Origin header never carries one, so this "+
				"entry could never match", o)
		}
		if u.Scheme != "https" && !isLocalHost(u.Hostname()) {
			// An http origin for a non-local host means the portal is served over
			// plaintext, and a bearer token that reaches this API from such a page
			// crossed the network in the clear.
			return fmt.Errorf("SENTINEL_ALLOWED_ORIGINS: %q must be https "+
				"unless it is a loopback development origin", o)
		}
	}
	return nil
}

func isLocalHost(host string) bool {
	return host == "localhost" || host == "127.0.0.1" || host == "::1"
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

func envBool(key string) bool {
	switch strings.ToLower(strings.TrimSpace(os.Getenv(key))) {
	case "1", "true", "yes", "on":
		return true
	}
	return false
}

func envFloat(key string, fallback float64) float64 {
	v, err := strconv.ParseFloat(strings.TrimSpace(os.Getenv(key)), 64)
	if err != nil {
		return fallback
	}
	return v
}

func envDuration(key string, fallback time.Duration) time.Duration {
	v, err := time.ParseDuration(strings.TrimSpace(os.Getenv(key)))
	if err != nil || v <= 0 {
		return fallback
	}
	return v
}

func splitList(s string) []string {
	var out []string
	for _, part := range strings.Split(s, ",") {
		if p := strings.TrimSpace(part); p != "" {
			out = append(out, p)
		}
	}
	return out
}

func main() {
	log := slog.New(slog.NewJSONHandler(os.Stdout, &slog.HandlerOptions{Level: slog.LevelInfo}))
	slog.SetDefault(log)

	if err := run(log); err != nil {
		log.Error("fatal", "error", err)
		os.Exit(1)
	}
}

func run(log *slog.Logger) error {
	cfg, err := loadConfig()
	if err != nil {
		return err
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	// Telemetry first, so anything that fails after this point is traced. Setup is
	// a no-op unless SENTINEL_OTEL_ENABLED is set, and the instruments built below
	// are no-ops in that case — nothing else in the process tests a flag.
	tel, err := telemetry.Setup(ctx, cfg.otel)
	if err != nil {
		return err
	}
	defer func() {
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if err := tel.Shutdown(shutdownCtx); err != nil {
			log.Warn("telemetry shutdown", "error", err)
		}
	}()
	metrics := telemetry.NewMetrics()
	log.Info("telemetry", "enabled", tel.Enabled(), "endpoint", cfg.otel.Endpoint)

	st, err := store.Open(ctx, cfg.databaseURL)
	if err != nil {
		return fmt.Errorf("database: %w", err)
	}
	defer st.Close()

	verifier := &auth.Verifier{
		Keys: &auth.CachingKeySource{
			URL:    auth.GoogleJWKSURL,
			Client: &http.Client{Timeout: 10 * time.Second},
			TTL:    time.Hour,
		},
		Issuer:   fmt.Sprintf(auth.GoogleIssuer, cfg.projectID),
		Audience: cfg.projectID,
		Leeway:   30 * time.Second,
	}

	objects, err := openObjectStore(ctx, cfg, log)
	if err != nil {
		return err
	}

	signer, err := ca.Load(cfg.ca)
	if err != nil {
		return err
	}
	log.Info("device certificate authority loaded",
		"subject", signer.Subject(), "not_after", signer.NotAfter().UTC())
	// A warning rather than a refusal: an intermediate with three months left is
	// still issuing valid year-long certificates, and refusing to start would take
	// the floor down over a rotation that has weeks of slack. But it needs saying
	// out loud, because the day the intermediate expires every device stops
	// connecting at once (see ca.FileCA.checkValidityWindow).
	if remaining := time.Until(signer.NotAfter()); remaining < ca.MinValidity {
		log.Warn("the device CA expires soon; rotate it before it starts shortening "+
			"or refusing device certificates",
			"not_after", signer.NotAfter().UTC(), "remaining", remaining.Round(time.Hour))
	}

	broker, err := buildTokenBroker(cfg, log)
	if err != nil {
		return err
	}

	srv := &api.Server{
		Log:      log,
		Store:    st,
		Verifier: verifier,
		Version:  version,
		CA:       signer,
		// The live floor view. The whole single-use-ticket mechanism in
		// internal/api/live.go was already built and correct; leaving this nil is
		// what made GET /v1/teams/{id}/live answer 503 on every deployment.
		//
		// A 60-second TTL because that is the window the ticket has to survive:
		// the portal mints one and immediately opens an EventSource with it.
		LiveTickets:    api.NewLiveTickets(60 * time.Second),
		AllowedOrigins: cfg.allowedOrigins,
		TokenBroker:    broker,
		TokenLimiter:   httpx.NewRateLimiter(cfg.tokenRate, cfg.tokenBurst),
		Readiness:      readinessChecks(st, objects),
		Metrics:        metrics,
	}
	if len(cfg.allowedOrigins) > 0 {
		log.Info("CORS enabled", "origins", cfg.allowedOrigins)
	}

	srv.Ingest = &ingest.Handler{
		Log:     log,
		Metrics: metrics,
		NewSink: func(peer ingest.Peer) ingest.Sink {
			return &ingest.DBSink{
				Store: st, Blob: objects, Tenant: peer.TenantID, Ctx: ctx, Metrics: metrics,
			}
		},
		PolicyVer: func(ctx context.Context, tenantID string) int64 {
			p, err := st.PolicyForTenant(ctx, tenantID)
			if err != nil {
				return 0
			}
			return p.Version
		},
		DeviceActive: func(ctx context.Context, tenantID, deviceID string) bool {
			status, err := st.DeviceStatus(ctx, tenantID, deviceID)
			// A database blip must not disconnect a live capture; only a definite
			// "revoked" does. The 60 s revocation window is measured from a
			// successful read.
			return err != nil || status == "active"
		},
	}

	// The finalize publisher. Its queue is written inside the ingest transaction
	// whether or not this is running, so a gateway with no broker configured loses
	// nothing — the messages accumulate and drain when one appears. That is worth
	// being loud about all the same: until then the pipeline receives no work, and
	// no call gets a transcript, an analysis or a compliance finding.
	if cfg.nats.URL == "" {
		log.Warn("no SENTINEL_NATS_URL: finalize messages will queue in Postgres " +
			"and the pipeline will receive no work until a broker is configured")
	} else {
		publisher, err := outbox.Dial(ctx, cfg.nats, log)
		if err != nil {
			return err
		}
		defer publisher.Close()
		drainer := &outbox.Drainer{
			Queue:     st,
			Publisher: publisher,
			Log:       log,
			Metrics:   metrics,
			Interval:  cfg.outboxInterval,
			Batch:     cfg.outboxBatch,
		}
		metrics.RegisterOutboxDepth(st.OutboxDepth)
		go drainer.Run(ctx)
	}

	httpSrv := &http.Server{
		Addr:              cfg.addr,
		Handler:           srv.Routes(),
		ReadHeaderTimeout: 10 * time.Second,
		// No write timeout: /v1/ingest and the SSE floor view are long-lived.
		IdleTimeout: 5 * time.Minute,
	}

	if cfg.tlsCert != "" {
		tlsCfg, err := tlsConfig(cfg)
		if err != nil {
			return err
		}
		httpSrv.TLSConfig = tlsCfg
	}

	errc := make(chan error, 1)
	go func() {
		log.Info("gateway listening", "addr", cfg.addr, "version", version,
			"mtls", cfg.clientCAs != "")
		if httpSrv.TLSConfig != nil {
			errc <- httpSrv.ListenAndServeTLS(cfg.tlsCert, cfg.tlsKey)
		} else {
			errc <- httpSrv.ListenAndServe()
		}
	}()

	select {
	case err := <-errc:
		if errors.Is(err, http.ErrServerClosed) {
			return nil
		}
		return err
	case <-ctx.Done():
		log.Info("shutting down")
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
		defer cancel()
		return httpSrv.Shutdown(shutdownCtx)
	}
}

// openObjectStore picks the object store, preferring S3 when a bucket is named.
//
// The filesystem backend stays because it is what makes a local stack runnable
// without MinIO, and it keeps the loud warning it always had: a Dir store is a single
// machine's disk, so it does not survive the instance, does not replicate, and cannot
// enforce the encryption-at-rest or residency properties a bank's security review
// asks about.
func openObjectStore(ctx context.Context, cfg config, log *slog.Logger) (blob.Store, error) {
	if cfg.s3.Bucket != "" {
		s3store, err := blob.OpenS3(ctx, cfg.s3)
		if err != nil {
			return nil, err
		}
		log.Info("object storage: S3", "bucket", s3store.Bucket(),
			"region", s3store.Region(), "encrypted", s3store.Encrypted(),
			"endpoint", cfg.s3.Endpoint)
		if !s3store.Encrypted() {
			log.Warn("S3 server-side encryption is off; this is for MinIO in " +
				"development and must not be a production setting")
		}
		// OPEN-4 in docs/open-decisions.md records the working assumption that all
		// storage stays in an India region, pending written confirmation from the
		// bank client. Saying so at startup means a region change is visible in
		// the deploy's own logs rather than only in a Terraform diff nobody kept.
		if s3store.Region() != "ap-south-1" && s3store.Region() != "ap-south-2" {
			log.Warn("borrower call audio is being written outside India; "+
				"OPEN-4 assumes India-only residency and requires the bank "+
				"client's written approval for anything else",
				"region", s3store.Region())
		}
		return s3store, nil
	}
	if cfg.blobDir != "" {
		log.Warn("using filesystem object storage; not for production", "dir", cfg.blobDir)
		return blob.Dir{Root: cfg.blobDir}, nil
	}
	return nil, errors.New("object storage is not configured: set SENTINEL_S3_BUCKET " +
		"for production, or SENTINEL_BLOB_DIR for local development")
}

// buildTokenBroker configures POST /v1/oauth/token, or leaves it unconfigured.
//
// Returning nil rather than failing is deliberate, and it is the opposite of the
// choice made for the CA. Enrollment is on the critical path for every deployment;
// the token endpoint is only on the critical path once the desktop agent's PKCE login
// ships, and until then a gateway serving the portal has no use for it. So a missing
// API key leaves the route mounted and answering temporarily_unavailable, which is a
// state an operator can read, rather than refusing to boot a portal-only deployment.
//
// A half-configured broker is a different matter and does fail: a client id with no
// API key is someone in the middle of configuring this, and the failure they want is
// at deploy time.
func buildTokenBroker(cfg config, log *slog.Logger) (api.TokenBroker, error) {
	if cfg.idp.APIKey == "" && cfg.idp.ClientID == "" {
		log.Warn("no SENTINEL_IDP_API_KEY or SENTINEL_OIDC_CLIENT_ID: " +
			"POST /v1/oauth/token will report itself unavailable and the desktop " +
			"PKCE sign-in cannot complete")
		return nil, nil
	}
	broker, err := idp.New(cfg.idp)
	if err != nil {
		return nil, err
	}
	log.Info("token endpoint configured", "client_id", cfg.idp.ClientID,
		"idp_tenant", cfg.idp.TenantID != "",
		"confidential", cfg.idp.ClientSecret != "")
	return broker, nil
}

// readinessChecks are the probes GET /readyz runs.
//
// Both of them, not just the database. A gateway whose object store is unreachable
// still accepts the WebSocket upgrade, still authenticates the device, and then fails
// every segment write — so the desktop's audio stays unacked in its spool until the
// 72-hour bound evicts it. Nothing about that is visible from the client, which is
// built to keep retrying; it shows up as a coverage gap weeks later. Readiness is the
// place to catch it, because an unready instance stops being sent new ingest
// connections.
func readinessChecks(st *store.Store, objects blob.Store) []api.ReadyCheck {
	checks := []api.ReadyCheck{{
		Name: "database",
		Check: func(ctx context.Context) error {
			return st.Pool().Ping(ctx)
		},
	}}
	// Every backend in internal/blob implements Prober, so this is an interface
	// assertion rather than a type switch — and if a future backend does not, the
	// check is skipped rather than the gateway reporting ready on a store it never
	// examined. The name of the missing check is its own signal in the response
	// body.
	if prober, ok := objects.(blob.Prober); ok {
		checks = append(checks, api.ReadyCheck{
			Name:  "object_store",
			Check: prober.Ping,
		})
	}
	return checks
}

// tlsConfig requires TLS 1.3 and requests, but does not require, a client
// certificate.
//
// Requesting rather than requiring is deliberate: /v1/devices/enroll is reached by a
// machine that has no certificate yet, and the portal is reached by browsers that
// have none at all. Routes that need a device sit behind api.RequireDevice, which
// checks the verified certificate rather than the connection's TLS mode.
func tlsConfig(cfg config) (*tls.Config, error) {
	t := &tls.Config{MinVersion: tls.VersionTLS13}
	if cfg.clientCAs == "" {
		return t, nil
	}
	pool := x509.NewCertPool()
	for _, path := range strings.Split(cfg.clientCAs, ",") {
		pem, err := os.ReadFile(strings.TrimSpace(path))
		if err != nil {
			return nil, fmt.Errorf("client CA %s: %w", path, err)
		}
		if !pool.AppendCertsFromPEM(pem) {
			return nil, fmt.Errorf("client CA %s: no certificates found", path)
		}
	}
	t.ClientCAs = pool
	t.ClientAuth = tls.VerifyClientCertIfGiven
	return t, nil
}
