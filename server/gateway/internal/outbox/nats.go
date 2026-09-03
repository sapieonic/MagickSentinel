package outbox

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"errors"
	"fmt"
	"log/slog"
	"net/url"
	"os"
	"strings"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/nats-io/nats.go/jetstream"
)

// NATSConfig is the broker connection.
//
// There is currently no NATS authentication anywhere in this repository, and
// consumer.py defaults to `nats://127.0.0.1:4222` — an unauthenticated plaintext
// connection to localhost, which is fine for a developer and is not a thing to ship.
// So this type accepts every credential form NATS supports and Dial refuses to
// connect to anything off-box without one. Making the authenticated, TLS-capable
// path the *supported* path rather than an option is the point: the alternative is a
// deployment where anyone who can reach port 4222 can subscribe to every tenant's
// finalize stream and enumerate their call volumes.
type NATSConfig struct {
	// URL is one or more comma-separated server URLs. `tls://` selects TLS
	// directly; `nats://` upgrades if the server advertises it.
	URL string
	// CredsFile is an NGS/operator-mode `.creds` file holding a user JWT and an
	// NKey seed. This is the form to prefer: the credential is scoped by an
	// account-level authorisation the server enforces, so a leaked gateway
	// credential cannot subscribe to subjects the gateway does not publish to.
	CredsFile string
	// NKeySeedFile is a bare NKey seed, for a server configured with nkey users.
	NKeySeedFile string
	// Token and User/Password are the older forms, accepted because a customer's
	// existing NATS deployment may be configured that way and refusing would mean
	// refusing the deployment.
	Token    string
	User     string
	Password string
	// TLSCAFile trusts a private CA — normal for an internal broker. TLSCertFile
	// and TLSKeyFile present a client certificate for mutual TLS. The environment
	// variable names behind these three match the pipeline consumer's
	// (SENTINEL_NATS_CA, SENTINEL_NATS_CLIENT_CERT, SENTINEL_NATS_CLIENT_KEY), so
	// one broker is configured once rather than twice with two spellings.
	TLSCAFile   string
	TLSCertFile string
	TLSKeyFile  string
	// TLSHostname verifies the broker against a name that is not the address it
	// was reached on, which is what a NATS cluster behind a service IP needs.
	TLSHostname string
	// AllowInsecure is an operator saying, in as many words, that they mean to
	// connect to a remote broker with no credential. It exists because refusing
	// outright would block a customer whose broker is genuinely isolated, and it is
	// spelled out rather than inferred for the same reason the consumer spells it
	// out: SENTINEL_NATS_ALLOW_INSECURE.
	AllowInsecure bool
	// Name identifies this connection in `nats server report connections`, which
	// is what an operator looks at when asking who is publishing.
	Name string
}

func (c NATSConfig) hasCredential() bool {
	return c.CredsFile != "" || c.NKeySeedFile != "" || c.Token != "" ||
		c.User != "" || c.TLSCertFile != ""
}

// NATSPublisher publishes to JetStream and waits for the broker's acknowledgement.
type NATSPublisher struct {
	conn *nats.Conn
	js   jetstream.JetStream
	log  *slog.Logger
	// timeout bounds one publish. Bounded rather than inheriting the drainer's
	// context lifetime, because the drainer's context lives as long as the process:
	// a broker that accepts the TCP connection and then never answers would
	// otherwise block the drain loop indefinitely with a growing queue behind it.
	timeout time.Duration
}

var _ Publisher = (*NATSPublisher)(nil)

// Dial connects, ensures the stream exists, and returns a publisher.
//
// It fails rather than degrading. A gateway that started with a broken broker
// connection and carried on would accept ingest, finalize calls, and queue finalize
// messages that never leave — which is survivable, because the outbox is durable and
// drains when the broker returns, but it is not something to discover from a metric
// three days later. Failing at startup puts it in front of whoever ran the deploy.
func Dial(ctx context.Context, cfg NATSConfig, log *slog.Logger) (*NATSPublisher, error) {
	if cfg.URL == "" {
		return nil, errors.New("outbox: no NATS URL")
	}
	if err := requireCredentialOffBox(cfg); err != nil {
		return nil, err
	}

	name := cfg.Name
	if name == "" {
		name = "sentinel-gateway"
	}
	opts := []nats.Option{
		nats.Name(name),
		// Reconnect forever. The drainer's retry loop would cope with a connection
		// that stayed down, but a client that gives up reconnecting turns a
		// two-minute broker restart into a permanent outage requiring a gateway
		// restart to clear.
		nats.MaxReconnects(-1),
		nats.ReconnectWait(2 * time.Second),
		nats.ReconnectJitter(500*time.Millisecond, 2*time.Second),
		nats.Timeout(10 * time.Second),
		nats.DisconnectErrHandler(func(_ *nats.Conn, err error) {
			log.Warn("nats: disconnected; finalize messages are queueing in Postgres", "error", err)
		}),
		nats.ReconnectHandler(func(c *nats.Conn) {
			log.Info("nats: reconnected", "url", c.ConnectedUrlRedacted())
		}),
	}

	switch {
	case cfg.CredsFile != "":
		opts = append(opts, nats.UserCredentials(cfg.CredsFile))
	case cfg.NKeySeedFile != "":
		opt, err := nats.NkeyOptionFromSeed(cfg.NKeySeedFile)
		if err != nil {
			return nil, fmt.Errorf("outbox: nkey seed: %w", err)
		}
		opts = append(opts, opt)
	case cfg.User != "":
		opts = append(opts, nats.UserInfo(cfg.User, cfg.Password))
	case cfg.Token != "":
		opts = append(opts, nats.Token(cfg.Token))
	}

	tlsCfg, err := buildTLS(cfg)
	if err != nil {
		return nil, err
	}
	if tlsCfg != nil {
		opts = append(opts, nats.Secure(tlsCfg))
	}

	conn, err := nats.Connect(cfg.URL, opts...)
	if err != nil {
		return nil, fmt.Errorf("outbox: connect to NATS: %w", err)
	}

	js, err := jetstream.New(conn)
	if err != nil {
		conn.Close()
		return nil, fmt.Errorf("outbox: jetstream: %w", err)
	}
	if err := ensureStream(ctx, js, log); err != nil {
		conn.Close()
		return nil, err
	}

	log.Info("nats: connected", "url", conn.ConnectedUrlRedacted(),
		"stream", Stream, "subject", Subject, "tls", conn.TLSRequired())
	return &NATSPublisher{conn: conn, js: js, log: log, timeout: 10 * time.Second}, nil
}

// requireCredentialOffBox refuses an anonymous connection to a broker that is not on
// this machine.
//
// Loopback is exempted because that is the documented development setup and the
// consumer's own default. Anything else without a credential is refused with a
// message naming the options, because the failure it prevents is not a crash — it is
// a production deployment that works perfectly and is open to the network.
func requireCredentialOffBox(cfg NATSConfig) error {
	if cfg.hasCredential() || cfg.AllowInsecure {
		return nil
	}
	for _, raw := range strings.Split(cfg.URL, ",") {
		raw = strings.TrimSpace(raw)
		if raw == "" {
			continue
		}
		u, err := url.Parse(raw)
		if err != nil {
			return fmt.Errorf("outbox: unparseable NATS URL %q: %w", raw, err)
		}
		if !isLoopbackHost(u.Hostname()) {
			return fmt.Errorf("outbox: refusing to connect to %s with no credential: "+
				"anything that can reach this stream can enumerate every tenant's call "+
				"volumes and inject work that spends their model budget. Set "+
				"SENTINEL_NATS_CREDS (or SENTINEL_NATS_NKEY_SEED, or "+
				"SENTINEL_NATS_USER/SENTINEL_NATS_PASSWORD, or SENTINEL_NATS_TOKEN), or "+
				"SENTINEL_NATS_ALLOW_INSECURE=1 if the broker is genuinely trusted", u.Host)
		}
	}
	return nil
}

func isLoopbackHost(host string) bool {
	switch host {
	case "localhost", "127.0.0.1", "::1", "[::1]", "":
		return true
	}
	return strings.HasPrefix(host, "127.")
}

// buildTLS assembles a TLS configuration, or nil when the connection is to be left
// to NATS's own negotiation.
func buildTLS(cfg NATSConfig) (*tls.Config, error) {
	if cfg.TLSCAFile == "" && cfg.TLSCertFile == "" && cfg.TLSKeyFile == "" &&
		cfg.TLSHostname == "" {
		return nil, nil
	}
	t := &tls.Config{MinVersion: tls.VersionTLS12}
	if cfg.TLSCAFile != "" {
		pemBytes, err := os.ReadFile(cfg.TLSCAFile)
		if err != nil {
			return nil, fmt.Errorf("outbox: NATS CA %s: %w", cfg.TLSCAFile, err)
		}
		pool := x509.NewCertPool()
		if !pool.AppendCertsFromPEM(pemBytes) {
			return nil, fmt.Errorf("outbox: NATS CA %s: no certificates found", cfg.TLSCAFile)
		}
		t.RootCAs = pool
	}
	if (cfg.TLSCertFile == "") != (cfg.TLSKeyFile == "") {
		return nil, errors.New("outbox: a NATS client certificate needs both a certificate and a key")
	}
	if cfg.TLSCertFile != "" {
		pair, err := tls.LoadX509KeyPair(cfg.TLSCertFile, cfg.TLSKeyFile)
		if err != nil {
			return nil, fmt.Errorf("outbox: NATS client certificate: %w", err)
		}
		t.Certificates = []tls.Certificate{pair}
	}
	if cfg.TLSHostname != "" {
		t.ServerName = cfg.TLSHostname
	}
	// There is deliberately no InsecureSkipVerify escape hatch, matching the
	// consumer: the broker is reached over the customer's network, and a publisher
	// that accepts any certificate is a publisher whose finalize messages can be
	// harvested by anything on that path.
	return t, nil
}

// ensureStream creates SENTINEL if it is absent and leaves it alone if it is not.
//
// Deliberately not CreateOrUpdateStream. The stream's retention, replica count and
// storage class are operational decisions the customer's NATS operator makes, and a
// gateway that reasserted its own idea of them on every deploy would silently
// reconfigure a stream someone had tuned — dropping the replica count on a clustered
// broker, for instance, which loses messages rather than erroring. Creating a missing
// stream is a convenience for a fresh deployment; overwriting an existing one is a
// liberty.
func ensureStream(ctx context.Context, js jetstream.JetStream, log *slog.Logger) error {
	ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()

	if _, err := js.Stream(ctx, Stream); err == nil {
		return nil
	} else if !errors.Is(err, jetstream.ErrStreamNotFound) {
		return fmt.Errorf("outbox: look up stream %s: %w", Stream, err)
	}

	_, err := js.CreateStream(ctx, jetstream.StreamConfig{
		Name: Stream,
		// The filter covers the dead-letter subject as well, because consumer.py
		// republishes poison messages to sentinel.call.dlq through this stream.
		Subjects: []string{SubjectFilter},
		// WorkQueue would be tempting — each finalize is consumed once — but it
		// permits only one consumer per subject, and the dead-letter subject on
		// the same stream needs its own. Limits retention with a max age is the
		// shape that survives a second consumer being added.
		Retention: jetstream.LimitsPolicy,
		Storage:   jetstream.FileStorage,
		// Seven days. Long enough that a pipeline outage over a weekend does not
		// lose work, short enough that the broker is not a second copy of the
		// call record: the messages carry identifiers only, but a stream nobody
		// ages out is still state outside the retention policy.
		MaxAge: 7 * 24 * time.Hour,
		// Two minutes of duplicate suppression on Nats-Msg-Id, which is the call
		// id. This absorbs the common at-least-once case — a publish that
		// succeeded and whose outbox row we failed to mark, retried seconds
		// later. It is not relied on for correctness; see Drainer.publish.
		Duplicates: 2 * time.Minute,
	})
	if err != nil {
		// Racing with another replica's create is not a failure: the loser gets
		// "stream name already in use" and the stream exists either way.
		if errors.Is(err, jetstream.ErrStreamNameAlreadyInUse) {
			return nil
		}
		return fmt.Errorf("outbox: create stream %s: %w", Stream, err)
	}
	log.Info("nats: created stream", "stream", Stream, "subjects", SubjectFilter)
	return nil
}

// Publish sends one message and waits for the PubAck.
//
// Synchronous, not PublishAsync. The asynchronous form returns a future and lets the
// caller batch, which is faster and wrong here: the outbox marks a row published on
// the strength of this call returning nil, so nil has to mean the broker has the
// message on disk. A batched publish would let the drainer mark a hundred rows
// published on the strength of a hundred writes to a socket buffer, which is the
// original bug wearing a different hat.
func (p *NATSPublisher) Publish(ctx context.Context, subject, dedupeID string, payload []byte) error {
	ctx, cancel := context.WithTimeout(ctx, p.timeout)
	defer cancel()
	ack, err := p.js.Publish(ctx, subject, payload, jetstream.WithMsgID(dedupeID))
	if err != nil {
		return err
	}
	if ack == nil {
		return errors.New("outbox: broker returned no acknowledgement")
	}
	return nil
}

// Close drains the connection so anything already in flight is flushed.
func (p *NATSPublisher) Close() {
	if p == nil || p.conn == nil {
		return
	}
	if err := p.conn.Drain(); err != nil {
		p.conn.Close()
	}
}
