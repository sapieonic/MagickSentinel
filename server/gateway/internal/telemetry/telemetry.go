// Package telemetry wires OpenTelemetry traces and metrics over OTLP.
//
// Two properties shape everything here.
//
// **It is off by default and cannot break a deploy.** SENTINEL_OTEL_ENABLED has to be
// set before an exporter is constructed at all. When it is not, Setup returns a
// Telemetry whose Shutdown is a no-op and installs nothing, so the global providers
// stay the OpenTelemetry API's built-in no-ops. Call sites do not test a flag or a nil
// pointer — they record unconditionally into instruments that discard. A gateway that
// has never heard of a collector behaves exactly as it did before this package
// existed, which is the only acceptable answer for observability code in a product
// whose failure mode is losing call audio.
//
// **What may be an attribute is a compliance question, not a taste question.** See
// the comment on Metrics.
package telemetry

import (
	"context"
	"errors"
	"fmt"
	"os"
	"time"

	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/exporters/otlp/otlpmetric/otlpmetrichttp"
	"go.opentelemetry.io/otel/exporters/otlp/otlptrace/otlptracehttp"
	"go.opentelemetry.io/otel/propagation"
	sdkmetric "go.opentelemetry.io/otel/sdk/metric"
	"go.opentelemetry.io/otel/sdk/resource"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	semconv "go.opentelemetry.io/otel/semconv/v1.43.0"
)

// ServiceName is the value every span, metric and log line is grouped under in
// Grafana. Hard-coded rather than read from OTEL_SERVICE_NAME because dashboards and
// alert rules are written against it, and a deploy that renamed the service by
// setting one environment variable would silently orphan every one of them.
const ServiceName = "sentinel-gateway"

// Config is what the operator controls.
type Config struct {
	// Enabled gates the whole package. False means no exporters, no background
	// goroutines and no network traffic.
	Enabled bool
	// Endpoint is the OTLP collector, from the standard OTEL_EXPORTER_OTLP_ENDPOINT.
	// Required when Enabled: an enabled exporter with no endpoint would quietly
	// retry against localhost:4318 forever, and "telemetry is on but nothing
	// arrives" is a worse state to debug than a refusal at startup.
	Endpoint string
	// Insecure sends OTLP over plaintext HTTP. Right for a collector sidecar on
	// localhost; wrong for anything that crosses a network, because spans carry
	// tenant ids.
	Insecure bool
	// Version is the gateway build, from -ldflags.
	Version string
	// Environment distinguishes the production deployment from a staging one on the
	// same collector.
	Environment string
	// SampleRatio is the head sampling ratio for traces that arrive without a
	// parent decision. 1.0 keeps everything, which is the right default at this
	// scale: a few hundred desktops produce a request rate a collector will not
	// notice, and the traces worth having are the rare failures that a sampler
	// throws away first.
	SampleRatio float64
}

// Telemetry holds what has to be shut down.
type Telemetry struct {
	shutdown []func(context.Context) error
	enabled  bool
}

// Enabled reports whether exporters were actually installed, so the caller can say so
// in its startup log rather than leaving an operator to guess.
func (t *Telemetry) Enabled() bool { return t != nil && t.enabled }

// Setup installs the global tracer and meter providers.
//
// It returns a Telemetry even on failure paths that are recoverable, so a caller can
// always defer Shutdown without a nil check.
func Setup(ctx context.Context, cfg Config) (*Telemetry, error) {
	if !cfg.Enabled {
		return &Telemetry{}, nil
	}
	if cfg.Endpoint == "" {
		return &Telemetry{}, errors.New("telemetry: OTEL_EXPORTER_OTLP_ENDPOINT is required " +
			"when SENTINEL_OTEL_ENABLED is set")
	}

	res, err := buildResource(cfg)
	if err != nil {
		return &Telemetry{}, err
	}

	t := &Telemetry{enabled: true}

	traceOpts := []otlptracehttp.Option{otlptracehttp.WithEndpointURL(cfg.Endpoint)}
	if cfg.Insecure {
		traceOpts = append(traceOpts, otlptracehttp.WithInsecure())
	}
	traceExp, err := otlptracehttp.New(ctx, traceOpts...)
	if err != nil {
		return t, fmt.Errorf("telemetry: trace exporter: %w", err)
	}
	ratio := cfg.SampleRatio
	if ratio <= 0 {
		ratio = 1
	}
	tp := sdktrace.NewTracerProvider(
		sdktrace.WithResource(res),
		sdktrace.WithBatcher(traceExp),
		// ParentBased so a sampling decision made upstream is respected. Nothing
		// upstream makes one today, but the portal and the desktop agent both
		// eventually will, and a server that re-decides breaks a trace in half.
		sdktrace.WithSampler(sdktrace.ParentBased(sdktrace.TraceIDRatioBased(ratio))),
	)
	t.shutdown = append(t.shutdown, tp.Shutdown)
	otel.SetTracerProvider(tp)

	metricOpts := []otlpmetrichttp.Option{otlpmetrichttp.WithEndpointURL(cfg.Endpoint)}
	if cfg.Insecure {
		metricOpts = append(metricOpts, otlpmetrichttp.WithInsecure())
	}
	metricExp, err := otlpmetrichttp.New(ctx, metricOpts...)
	if err != nil {
		return t, fmt.Errorf("telemetry: metric exporter: %w", err)
	}
	mp := sdkmetric.NewMeterProvider(
		sdkmetric.WithResource(res),
		// Thirty seconds. Long enough that the export is negligible, short enough
		// that an outbox that stops draining is visible inside a shift change.
		sdkmetric.WithReader(sdkmetric.NewPeriodicReader(metricExp,
			sdkmetric.WithInterval(30*time.Second))),
	)
	t.shutdown = append(t.shutdown, mp.Shutdown)
	otel.SetMeterProvider(mp)

	// W3C trace context and baggage. The propagator is set even though nothing
	// upstream currently sends a traceparent, because the alternative — no
	// propagator — silently drops one if anything ever does.
	otel.SetTextMapPropagator(propagation.NewCompositeTextMapPropagator(
		propagation.TraceContext{}, propagation.Baggage{}))

	return t, nil
}

// Shutdown flushes pending spans and metrics.
//
// Every shutdown is attempted even if an earlier one fails: a trace exporter that
// cannot reach the collector must not stop the meter provider from being torn down,
// or the process hangs on exit waiting for a goroutine nobody stopped.
func (t *Telemetry) Shutdown(ctx context.Context) error {
	if t == nil {
		return nil
	}
	var errs []error
	for _, fn := range t.shutdown {
		if err := fn(ctx); err != nil {
			errs = append(errs, err)
		}
	}
	t.shutdown = nil
	return errors.Join(errs...)
}

func buildResource(cfg Config) (*resource.Resource, error) {
	attrs := []attribute.KeyValue{
		semconv.ServiceName(ServiceName),
		semconv.ServiceVersion(orUnknown(cfg.Version)),
	}
	if cfg.Environment != "" {
		attrs = append(attrs, semconv.DeploymentEnvironmentNameKey.String(cfg.Environment))
	}
	if host, err := os.Hostname(); err == nil && host != "" {
		// The pod or instance name. Useful and safe: it identifies our
		// infrastructure, not a person.
		attrs = append(attrs, semconv.HostName(host))
	}
	// resource.Merge against the default rather than resource.NewWithAttributes so
	// the SDK's own telemetry.sdk.* attributes survive; a collector uses them to
	// tell an instrumented service from a scraped one.
	res, err := resource.Merge(resource.Default(), resource.NewWithAttributes(semconv.SchemaURL, attrs...))
	if err != nil {
		return nil, fmt.Errorf("telemetry: build resource: %w", err)
	}
	return res, nil
}

func orUnknown(s string) string {
	if s == "" {
		return "unknown"
	}
	return s
}
