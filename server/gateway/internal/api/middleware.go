package api

import (
	"context"
	"crypto/x509"
	"errors"
	"net/http"
	"strings"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
	"github.com/magickvoice/sentinel/server/gateway/internal/httpx"
	"github.com/magickvoice/sentinel/server/gateway/internal/store"
)

// Authenticate verifies the bearer token and, when a client certificate is
// presented, the device behind it.
//
// The two identities are cross-checked here rather than in each handler: if the
// certificate says one tenant and the token says another, that is either a
// misconfiguration or an attack, and either way the request stops at the door.
func (s *Server) Authenticate(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		raw, err := auth.BearerToken(r)
		if err != nil {
			// Counted by reason rather than as one number, because the reasons
			// mean different things to whoever is on call: a spike in
			// token_invalid is usually the identity provider, a spike in
			// tenant_mismatch is an attack or a misprovisioned floor, and a spike
			// in device_revoked is a revocation nobody told the floor about.
			// See telemetry.Metrics on why there is no tenant label here: a
			// rejected request has no verified tenant, and using the unverified
			// one would let a caller mint label values at will.
			s.Metrics.AuthRejection(r.Context(), "no_token")
			httpx.WriteError(w, r, http.StatusUnauthorized, "unauthorized", "missing bearer token")
			return
		}
		id, err := s.Verifier.Verify(r.Context(), raw)
		if err != nil {
			s.Metrics.AuthRejection(r.Context(), "token_invalid")
			httpx.WriteError(w, r, http.StatusUnauthorized, "unauthorized", "token rejected")
			return
		}
		if cert := clientCert(r); cert != nil {
			if err := s.attachDevice(r.Context(), id, cert); err != nil {
				switch {
				case errors.Is(err, auth.ErrDeviceRevoked):
					s.Metrics.AuthRejection(r.Context(), "device_revoked")
					httpx.WriteError(w, r, http.StatusForbidden, "device_revoked",
						"this device has been revoked")
				case errors.Is(err, auth.ErrTenantMismatch):
					s.Metrics.AuthRejection(r.Context(), "tenant_mismatch")
					httpx.WriteError(w, r, http.StatusForbidden, "tenant_mismatch",
						"device and user belong to different tenants")
				default:
					s.Metrics.AuthRejection(r.Context(), "device_unknown")
					httpx.WriteError(w, r, http.StatusForbidden, "device_unknown",
						"device certificate not recognised")
				}
				return
			}
		}
		next.ServeHTTP(w, r.WithContext(auth.WithIdentity(r.Context(), id)))
	})
}

func clientCert(r *http.Request) *x509.Certificate {
	if r.TLS != nil && len(r.TLS.PeerCertificates) > 0 {
		return r.TLS.PeerCertificates[0]
	}
	return nil
}

func (s *Server) attachDevice(ctx context.Context, id *auth.Identity, cert *x509.Certificate) error {
	fp := auth.CertFingerprint(cert)
	deviceID, tenantID, status, err := s.Store.DeviceByCertFingerprint(ctx, fp)
	if err != nil {
		if errors.Is(err, store.ErrNotFound) {
			return auth.ErrNoDeviceCert
		}
		return err
	}
	if status != "active" {
		return auth.ErrDeviceRevoked
	}
	if tenantID != id.TenantID {
		return auth.ErrTenantMismatch
	}
	id.DeviceID, id.CertFingerprint = deviceID, fp
	return nil
}

// RequireDevice rejects routes that need mutual TLS when no device was attached.
// /v1/policy, /v1/heartbeat and /v1/ingest are device-scoped: a valid user token
// alone must not be enough to read a tenant's capture configuration.
//
// Kept as a free function because docs/security.md names it, and because a
// middleware that needs no server state should not require one. The Server's own
// routes go through Server.requireDevice, which is the same check with the rejection
// counted.
func RequireDevice(next http.Handler) http.Handler {
	return requireDevice(nil, next)
}

func (s *Server) requireDevice(next http.Handler) http.Handler {
	return requireDevice(s, next)
}

func requireDevice(s *Server, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if id := auth.FromContext(r.Context()); id == nil || id.DeviceID == "" {
			if s != nil {
				s.Metrics.AuthRejection(r.Context(), "device_required")
			}
			httpx.WriteError(w, r, http.StatusForbidden, "device_required",
				"this endpoint requires an enrolled device certificate")
			return
		}
		next.ServeHTTP(w, r)
	})
}

// AssertMeNamespace is the middleware the /v1/me contract depends on.
//
// The `me` namespace exists so the desktop binary can never be repointed at another
// agent's data. The rule is enforced here, mechanically, rather than by each handler
// remembering to read the UID from the token: any /v1/me route that carries a user
// identifier in the path or query is rejected outright, so the only UID a handler can
// possibly use is the verified one.
func AssertMeNamespace(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !strings.HasPrefix(r.URL.Path, "/v1/me") {
			next.ServeHTTP(w, r)
			return
		}
		q := r.URL.Query()
		for _, k := range []string{"user_uid", "uid", "user", "agent_id", "as_user"} {
			if q.Has(k) {
				httpx.WriteError(w, r, http.StatusBadRequest, "uid_not_accepted",
					"the me namespace derives the user from the token; "+k+" is not accepted")
				return
			}
		}
		next.ServeHTTP(w, r)
	})
}
