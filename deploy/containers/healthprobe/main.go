// Command healthprobe is the gateway image's HEALTHCHECK.
//
// WHY A BINARY AND NOT `curl` OR `wget`.
//
// The gateway image's final stage is distroless: no shell, no busybox, no package
// manager, nothing but the statically linked gateway. That is the right base for a
// service that terminates TLS, holds database credentials and answers requests from
// the public internet — the attack surface is one binary, and there is nothing for a
// remote-code-execution primitive to exec.
//
// Docker's HEALTHCHECK, though, runs a command inside the container. With no shell
// and no HTTP client there is nothing to run. The usual workarounds are all worse
// than this file:
//
//   * Switch the base to alpine so busybox wget is available. That adds a shell to a
//     production image for the sole benefit of the orchestrator, which is a poor
//     trade — and the shell is the first thing a security reviewer asks about.
//   * Drop HEALTHCHECK and rely on the orchestrator's own probe. Fine on Kubernetes,
//     useless with `docker compose`, which is the local/dev stack this repository has.
//   * Have the gateway probe itself with a `-healthcheck` flag. That means editing
//     server/gateway, which this work stream does not own.
//
// So: a few hundred lines of standard library, compiled in the same builder stage
// from the same toolchain, copied into the final image, no dependencies.
//
// WHAT IT PROBES, AND THE ONE DELIBERATE TOLERANCE.
//
// `/healthz` exists today (server/gateway/internal/api/server.go) and answers
// {"status":"ok","version":...} outside authentication, which is exactly what a probe
// needs. It is required: a non-2xx or an unreachable port is unhealthy, full stop.
//
// `/readyz` is being added by another work stream and does not exist yet. A probe
// that required it would report every gateway image unhealthy until that lands; a
// probe that ignored it would still be ignoring it a year later. So: a 404 from an
// optional path is treated as "not implemented yet" and passes, and any other
// non-2xx — 500, 503, a timeout — fails. When /readyz lands, drop `-tolerate-404`
// from the Dockerfile's HEALTHCHECK and the tolerance is gone.
//
// The distinction matters more than it looks. /healthz says the process is up.
// /readyz says it can reach its dependencies. A gateway that is up but cannot reach
// Postgres accepts an ingest connection, buffers audio, and fails every write — and
// the endpoint agent, which never deletes a segment until the server acks it, spools
// until it hits the 2 GB / 72 h cap and starts evicting. Liveness alone cannot see
// that; readiness can.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"strings"
	"time"
)

func main() {
	var (
		base     = flag.String("base", "http://127.0.0.1:8080", "base URL to probe")
		required = flag.String("required", "/healthz", "comma-separated paths that must return 2xx")
		optional = flag.String("optional", "", "comma-separated paths that may be absent")
		tolerate = flag.Bool("tolerate-404", false, "treat 404 on an optional path as not-yet-implemented rather than unhealthy")
		timeout  = flag.Duration("timeout", 3*time.Second, "per-request timeout")
	)
	flag.Parse()

	// One client, no redirects followed. A health endpoint that 302s is a
	// misconfiguration and should read as unhealthy rather than being chased.
	client := &http.Client{
		Timeout: *timeout,
		CheckRedirect: func(*http.Request, []*http.Request) error {
			return errors.New("health endpoint redirected; treating as unhealthy")
		},
		Transport: &http.Transport{
			DialContext:       (&net.Dialer{Timeout: *timeout}).DialContext,
			DisableKeepAlives: true,
		},
	}

	var failures []string

	for _, p := range split(*required) {
		if err := probe(client, *base+p, *timeout, false, false); err != nil {
			failures = append(failures, fmt.Sprintf("%s: %v", p, err))
		}
	}
	for _, p := range split(*optional) {
		if err := probe(client, *base+p, *timeout, true, *tolerate); err != nil {
			failures = append(failures, fmt.Sprintf("%s: %v", p, err))
		}
	}

	if len(failures) > 0 {
		// stderr, and exit 1: `docker inspect` surfaces the last few health-check
		// outputs, and "which path failed and why" is the whole diagnostic value of
		// having run a probe at all.
		fmt.Fprintf(os.Stderr, "unhealthy: %s\n", strings.Join(failures, "; "))
		os.Exit(1)
	}
	fmt.Println("ok")
}

func probe(client *http.Client, url string, timeout time.Duration, optional, tolerate404 bool) error {
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return err
	}
	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	// Drain so the connection can be reused or closed cleanly rather than being
	// abandoned mid-response, which shows up in the gateway's logs as a client error
	// every health-check interval.
	_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 4096))

	if resp.StatusCode >= 200 && resp.StatusCode < 300 {
		return nil
	}
	if optional && tolerate404 && resp.StatusCode == http.StatusNotFound {
		// Not yet implemented. Deliberately noisy on stdout so it is visible in
		// `docker inspect` output that the container is passing with a tolerance
		// applied, rather than passing outright.
		fmt.Printf("note: %s returned 404 and is treated as not-yet-implemented\n", url)
		return nil
	}
	return fmt.Errorf("status %d", resp.StatusCode)
}

func split(s string) []string {
	var out []string
	for _, p := range strings.Split(s, ",") {
		if p = strings.TrimSpace(p); p != "" {
			out = append(out, p)
		}
	}
	return out
}
