package workload

import (
	"bytes"
	"embed"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"maps"
	"net"
	"net/http"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/events"
	"github.com/tinylib/msgp/msgp"
)

//go:embed fixtures/*.json
var fixtureFiles embed.FS

// Senders performs the network actions of the workload.
//
// Each method also returns the moment the item went on the wire, taken after any
// wait for the receiver and just before the write or the request. The moment is the
// zero time when nothing was sent.
type Senders interface {
	DogStatsD(lines []string) (time.Time, error)
	Spans(fixture string) (time.Time, error)
	OTLP(fixture string) (time.Time, error)
	HTTPGet(url string) (status int, sentAt time.Time, err error)
}

// RealSenders sends to the serverless-init listeners. Zero values select the
// default addresses and timings.
type RealSenders struct {
	DogStatsDAddress string        // default 127.0.0.1:8125
	TraceURL         string        // default http://127.0.0.1:8126
	PollInterval     time.Duration // default 100 milliseconds
	PollTimeout      time.Duration // default 10 seconds
	HTTPTimeout      time.Duration // default 5 seconds, for each HTTP request
	// OnReceiverReady runs once, when the trace receiver first answers on /info.
	OnReceiverReady func()

	ready sync.Once
}

// NewRealSenders returns senders with the default addresses. They append a
// receiver_ready event to eventLog, and report a failure to write it on errOut.
func NewRealSenders(eventLog string, errOut io.Writer) *RealSenders {
	return &RealSenders{OnReceiverReady: func() {
		e := events.Event{Time: time.Now(), Kind: "receiver_ready"}
		if err := events.Append(eventLog, e); err != nil {
			fmt.Fprintf(errOut, "%scannot write event %q: %v\n", childMarker, e.Kind, err)
		}
	}}
}

// client returns the HTTP client, whose timeout keeps a listener that never answers from holding
// the workload.
func (r *RealSenders) client() *http.Client {
	timeout := r.HTTPTimeout
	if timeout == 0 {
		timeout = 5 * time.Second
	}
	return &http.Client{Timeout: timeout}
}

func (r *RealSenders) traceURL() string {
	if r.TraceURL == "" {
		return "http://127.0.0.1:8126"
	}
	return r.TraceURL
}

// DogStatsD sends the lines in one UDP datagram, separated by newlines.
func (r *RealSenders) DogStatsD(lines []string) (time.Time, error) {
	address := r.DogStatsDAddress
	if address == "" {
		address = "127.0.0.1:8125"
	}
	conn, err := net.Dial("udp", address)
	if err != nil {
		return time.Time{}, err
	}
	defer conn.Close()
	sentAt := time.Now()
	_, err = conn.Write([]byte(strings.Join(lines, "\n")))
	return sentAt, err
}

// Spans waits once for the trace receiver, then posts the fixture.
func (r *RealSenders) Spans(fixture string) (time.Time, error) {
	payload, traceCount, err := encodeSpanFixture(fixture)
	if err != nil {
		return time.Time{}, err
	}
	var readyErr error
	r.ready.Do(func() {
		if readyErr = r.waitForReceiver(); readyErr == nil && r.OnReceiverReady != nil {
			r.OnReceiverReady()
		}
	})
	if readyErr != nil {
		return time.Time{}, readyErr
	}
	request, err := http.NewRequest(http.MethodPost, r.traceURL()+"/v0.4/traces", bytes.NewReader(payload))
	if err != nil {
		return time.Time{}, err
	}
	request.Header.Set("Content-Type", "application/msgpack")
	request.Header.Set("X-Datadog-Trace-Count", fmt.Sprint(traceCount))
	sentAt := time.Now()
	response, err := r.client().Do(request)
	if err != nil {
		return sentAt, err
	}
	defer response.Body.Close()
	io.Copy(io.Discard, response.Body)
	if response.StatusCode/100 != 2 {
		return sentAt, fmt.Errorf("trace receiver answered status %d", response.StatusCode)
	}
	return sentAt, nil
}

func (r *RealSenders) waitForReceiver() error {
	interval, timeout := r.PollInterval, r.PollTimeout
	if interval == 0 {
		interval = 100 * time.Millisecond
	}
	if timeout == 0 {
		timeout = 10 * time.Second
	}
	deadline := time.Now().Add(timeout)
	for {
		response, err := r.client().Get(r.traceURL() + "/info")
		if err == nil {
			response.Body.Close()
			if response.StatusCode == http.StatusOK {
				return nil
			}
		}
		if time.Now().After(deadline) {
			return fmt.Errorf("trace receiver did not answer /info within %s", timeout)
		}
		time.Sleep(interval)
	}
}

// OTLP is not implemented yet.
func (r *RealSenders) OTLP(fixture string) (time.Time, error) {
	return time.Time{}, errors.New("send_otlp is not implemented yet")
}

// HTTPGet requests url and returns the response status code.
func (r *RealSenders) HTTPGet(url string) (int, time.Time, error) {
	sentAt := time.Now()
	response, err := r.client().Get(url)
	if err != nil {
		return 0, sentAt, err
	}
	defer response.Body.Close()
	io.Copy(io.Discard, response.Body)
	return response.StatusCode, sentAt, nil
}

type fixtureSpan struct {
	Service  string             `json:"service"`
	Name     string             `json:"name"`
	Resource string             `json:"resource"`
	TraceID  uint64             `json:"trace_id"`
	SpanID   uint64             `json:"span_id"`
	ParentID uint64             `json:"parent_id"`
	Start    int64              `json:"start"`
	Duration int64              `json:"duration"`
	Error    int32              `json:"error"`
	Type     string             `json:"type"`
	Meta     map[string]string  `json:"meta"`
	Metrics  map[string]float64 `json:"metrics"`
}

type spanFixture struct {
	Encoding string          `json:"encoding"`
	Traces   [][]fixtureSpan `json:"traces"`
}

func readSpanFixture(name string) (spanFixture, error) {
	var fixture spanFixture
	data, err := fixtureFiles.ReadFile("fixtures/" + name + ".json")
	if err != nil {
		return fixture, fmt.Errorf("span fixture %q: %w", name, err)
	}
	if err := json.Unmarshal(data, &fixture); err != nil {
		return fixture, fmt.Errorf("span fixture %q: %w", name, err)
	}
	return fixture, nil
}

// FixtureResources returns the resource of every span in the named fixture, in fixture order.
func FixtureResources(name string) ([]string, error) {
	fixture, err := readSpanFixture(name)
	if err != nil {
		return nil, err
	}
	var out []string
	for _, trace := range fixture.Traces {
		for _, span := range trace {
			out = append(out, span.Resource)
		}
	}
	return out, nil
}

// encodeSpanFixture reads the named fixture and returns the /v0.4 msgpack payload
// (an array of traces, each an array of span maps) and the number of traces.
func encodeSpanFixture(name string) ([]byte, int, error) {
	fixture, err := readSpanFixture(name)
	if err != nil {
		return nil, 0, err
	}
	if fixture.Encoding != "v04" {
		return nil, 0, fmt.Errorf("span fixture %q: encoding %q is not supported yet, only v04", name, fixture.Encoding)
	}
	var out []byte
	out = msgp.AppendArrayHeader(out, uint32(len(fixture.Traces)))
	for _, trace := range fixture.Traces {
		out = msgp.AppendArrayHeader(out, uint32(len(trace)))
		for _, span := range trace {
			out = appendSpanV04(out, span)
		}
	}
	return out, len(fixture.Traces), nil
}

func appendSpanV04(out []byte, s fixtureSpan) []byte {
	out = msgp.AppendMapHeader(out, 12)
	out = msgp.AppendString(out, "service")
	out = msgp.AppendString(out, s.Service)
	out = msgp.AppendString(out, "name")
	out = msgp.AppendString(out, s.Name)
	out = msgp.AppendString(out, "resource")
	out = msgp.AppendString(out, s.Resource)
	out = msgp.AppendString(out, "trace_id")
	out = msgp.AppendUint64(out, s.TraceID)
	out = msgp.AppendString(out, "span_id")
	out = msgp.AppendUint64(out, s.SpanID)
	out = msgp.AppendString(out, "parent_id")
	out = msgp.AppendUint64(out, s.ParentID)
	out = msgp.AppendString(out, "start")
	out = msgp.AppendInt64(out, s.Start)
	out = msgp.AppendString(out, "duration")
	out = msgp.AppendInt64(out, s.Duration)
	out = msgp.AppendString(out, "error")
	out = msgp.AppendInt32(out, s.Error)
	out = msgp.AppendString(out, "type")
	out = msgp.AppendString(out, s.Type)
	out = msgp.AppendString(out, "meta")
	out = appendSortedMap(out, s.Meta, msgp.AppendString)
	out = msgp.AppendString(out, "metrics")
	out = appendSortedMap(out, s.Metrics, msgp.AppendFloat64)
	return out
}

// appendSortedMap writes m with its keys in sorted order, so the payload is the same on every run.
func appendSortedMap[V any](out []byte, m map[string]V, appendValue func([]byte, V) []byte) []byte {
	out = msgp.AppendMapHeader(out, uint32(len(m)))
	for _, key := range slices.Sorted(maps.Keys(m)) {
		out = msgp.AppendString(out, key)
		out = appendValue(out, m[key])
	}
	return out
}
