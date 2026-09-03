package httpx

import (
	"net"
	"net/http"
	"strings"
	"sync"
	"time"
)

// RateLimiter is a per-client token bucket.
//
// It exists for the routes that sit outside the authentication middleware, of which
// there is currently one that matters: POST /v1/oauth/token. Everything behind
// Authenticate is already bounded by needing a signed Identity Platform token, and
// the enrollment endpoint is bounded by needing a single-use token that an admin
// minted. The token endpoint is bounded by nothing — it is reachable by anyone who
// can resolve the hostname — and it brokers to an upstream identity provider using a
// credential we hold, so an unbounded one is both a way to burn our own IdP quota and
// a way to use us as an oracle against Google's rate limits.
//
// Deliberately in-process and deliberately approximate. A shared limiter in Redis
// would be more correct behind several replicas, and it would also mean the token
// endpoint stops working when Redis does — trading a bounded abuse problem for an
// unbounded availability one. Per-replica limits multiply by the replica count, which
// is a factor of two or three, not a factor that matters.
type RateLimiter struct {
	// Rate is the sustained requests per second allowed per client.
	rate float64
	// burst is how many requests a client may make back to back. A desktop signing
	// in makes one token request, and a token refresh is one more every fifty
	// minutes, so a burst in the low single digits is generous for the real
	// client and still tight.
	burst float64

	mu      sync.Mutex
	buckets map[string]*bucket
	// lastSweep bounds the map. The keys are client addresses, which an attacker
	// controls the number of, so an unswept map is a memory-exhaustion vector
	// dressed up as a defence against abuse.
	lastSweep time.Time
	now       func() time.Time
}

type bucket struct {
	tokens float64
	last   time.Time
}

// NewRateLimiter builds a limiter allowing `rate` requests per second per client with
// a burst of `burst`.
func NewRateLimiter(rate, burst float64) *RateLimiter {
	if rate <= 0 {
		rate = 1
	}
	if burst < 1 {
		burst = 1
	}
	return &RateLimiter{rate: rate, burst: burst, buckets: map[string]*bucket{}}
}

func (l *RateLimiter) clock() time.Time {
	if l.now != nil {
		return l.now()
	}
	return time.Now()
}

// Allow reports whether a request from key may proceed.
func (l *RateLimiter) Allow(key string) bool {
	now := l.clock()
	l.mu.Lock()
	defer l.mu.Unlock()

	// Sweep on write, like the live-view ticket store: a background reaper for a
	// map this small is more machinery than the problem deserves, and sweeping only
	// when the map has had a minute to accumulate keeps the common path to one map
	// lookup.
	if now.Sub(l.lastSweep) > time.Minute {
		full := l.burst / l.rate
		for k, b := range l.buckets {
			// A bucket that has had time to refill completely is
			// indistinguishable from a new one, so it carries no information and
			// can go.
			if now.Sub(b.last) > time.Duration(full*float64(time.Second))+time.Minute {
				delete(l.buckets, k)
			}
		}
		l.lastSweep = now
	}

	b, ok := l.buckets[key]
	if !ok {
		l.buckets[key] = &bucket{tokens: l.burst - 1, last: now}
		return true
	}
	b.tokens += now.Sub(b.last).Seconds() * l.rate
	if b.tokens > l.burst {
		b.tokens = l.burst
	}
	b.last = now
	if b.tokens < 1 {
		return false
	}
	b.tokens--
	return true
}

// ClientKey identifies the caller for rate-limiting purposes.
//
// The remote address, and only the remote address. X-Forwarded-For is deliberately
// ignored: it is a header the client sets, so honouring it would let anyone bypass
// the limit entirely by varying one string. A deployment behind a load balancer
// therefore sees every request as coming from the balancer and rate-limits the
// balancer as one client — which is wrong, and is the correct wrong: it fails closed
// (too strict) rather than open (no limit at all). Fixing it properly means the
// balancer's address being on a trust list, which is a configuration item to add when
// there is a load balancer to configure it for.
func ClientKey(r *http.Request) string {
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil {
		return strings.TrimSpace(r.RemoteAddr)
	}
	return host
}

// RateLimit rejects requests over the limit before they reach the handler.
//
// The response is the standard error envelope with a Retry-After, rather than a bare
// 429: a desktop agent that gets rate-limited needs to know to back off, and its
// error path (client/sentinel-agent/src/api.rs) distinguishes status codes rather
// than parsing bodies, so the status is what carries the meaning.
func RateLimit(l *RateLimiter, retryAfter time.Duration, next http.Handler) http.Handler {
	if l == nil {
		return next
	}
	seconds := int(retryAfter.Seconds())
	if seconds < 1 {
		seconds = 1
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !l.Allow(ClientKey(r)) {
			w.Header().Set("Retry-After", itoa(seconds))
			WriteError(w, r, http.StatusTooManyRequests, "rate_limited",
				"too many requests; retry after a short delay")
			return
		}
		next.ServeHTTP(w, r)
	})
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	var buf [20]byte
	i := len(buf)
	for n > 0 {
		i--
		buf[i] = byte('0' + n%10)
		n /= 10
	}
	return string(buf[i:])
}
