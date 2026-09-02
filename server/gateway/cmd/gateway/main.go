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
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"github.com/magickvoice/sentinel/server/gateway/internal/api"
	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
	"github.com/magickvoice/sentinel/server/gateway/internal/blob"
	"github.com/magickvoice/sentinel/server/gateway/internal/ingest"
	"github.com/magickvoice/sentinel/server/gateway/internal/store"
)

// version is set at build time with -ldflags "-X main.version=...".
var version = "dev"

type config struct {
	addr        string
	databaseURL string
	projectID   string
	blobDir     string
	tlsCert     string
	tlsKey      string
	clientCAs   string
}

func loadConfig() (config, error) {
	c := config{
		addr:        envOr("SENTINEL_ADDR", ":8080"),
		databaseURL: os.Getenv("SENTINEL_DATABASE_URL"),
		projectID:   os.Getenv("SENTINEL_GCP_PROJECT"),
		blobDir:     envOr("SENTINEL_BLOB_DIR", ""),
		tlsCert:     os.Getenv("SENTINEL_TLS_CERT"),
		tlsKey:      os.Getenv("SENTINEL_TLS_KEY"),
		clientCAs:   os.Getenv("SENTINEL_CLIENT_CA"),
	}
	if c.databaseURL == "" {
		return c, errors.New("SENTINEL_DATABASE_URL is required")
	}
	if c.projectID == "" {
		return c, errors.New("SENTINEL_GCP_PROJECT is required to verify ID tokens")
	}
	return c, nil
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
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

	var objects blob.Store
	if cfg.blobDir != "" {
		objects = blob.Dir{Root: cfg.blobDir}
		log.Warn("using filesystem object storage; not for production", "dir", cfg.blobDir)
	} else {
		return errors.New("SENTINEL_BLOB_DIR is required until the S3 adapter is configured")
	}

	srv := &api.Server{Log: log, Store: st, Verifier: verifier, Version: version}
	srv.Ingest = &ingest.Handler{
		Log: log,
		NewSink: func(peer ingest.Peer) ingest.Sink {
			return &ingest.DBSink{Store: st, Blob: objects, Tenant: peer.TenantID, Ctx: ctx}
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
