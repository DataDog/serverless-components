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
	encoded, err := encodeSpanFixture(fixture)
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
	request, err := http.NewRequest(http.MethodPost, r.traceURL()+encoded.path, bytes.NewReader(encoded.body))
	if err != nil {
		return time.Time{}, err
	}
	request.Header.Set("Content-Type", "application/msgpack")
	request.Header.Set("X-Datadog-Trace-Count", fmt.Sprint(encoded.traceCount))
	for name, value := range encoded.headers {
		request.Header.Set(name, value)
	}
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

// fixtureTracerPayload holds the tracer fields of a v07 payload body.
type fixtureTracerPayload struct {
	LanguageName    string `json:"language_name"`
	LanguageVersion string `json:"language_version"`
	TracerVersion   string `json:"tracer_version"`
	RuntimeID       string `json:"runtime_id"`
	Env             string `json:"env"`
	Hostname        string `json:"hostname"`
	AppVersion      string `json:"app_version"`
}

type spanFixture struct {
	Encoding string `json:"encoding"`
	// Headers are added to the request, such as the tracer metadata headers a tracer sends.
	Headers       map[string]string    `json:"headers"`
	TracerPayload fixtureTracerPayload `json:"tracer_payload"`
	Traces        [][]fixtureSpan      `json:"traces"`
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

// encodedFixture is a span fixture ready to post to the trace receiver.
type encodedFixture struct {
	path       string // the receiver endpoint for the fixture's encoding
	body       []byte
	traceCount int
	headers    map[string]string // the fixture's extra request headers
}

// encodeSpanFixture reads the named fixture and encodes it in the msgpack format its
// "encoding" field names:
//   - v04: an array of traces, each an array of span maps.
//   - v05: a two-element array of the string table and the traces, each an array of
//     12-element span arrays whose strings are indexes into the table.
//   - v07: a tracer payload map with the fixture's tracer fields, whose chunks hold the spans
//     as v04 span maps.
func encodeSpanFixture(name string) (encodedFixture, error) {
	fixture, err := readSpanFixture(name)
	if err != nil {
		return encodedFixture{}, err
	}
	encoded := encodedFixture{traceCount: len(fixture.Traces), headers: fixture.Headers}
	switch fixture.Encoding {
	case "v04":
		encoded.path, encoded.body = "/v0.4/traces", encodeV04(fixture.Traces)
	case "v05":
		encoded.path, encoded.body = "/v0.5/traces", encodeV05(fixture.Traces)
	case "v07":
		encoded.path, encoded.body = "/v0.7/traces", encodeV07(fixture.TracerPayload, fixture.Traces)
	default:
		return encodedFixture{}, fmt.Errorf("span fixture %q: encoding %q is not supported, only v04, v05 and v07", name, fixture.Encoding)
	}
	return encoded, nil
}

func encodeV04(traces [][]fixtureSpan) []byte {
	out := msgp.AppendArrayHeader(nil, uint32(len(traces)))
	for _, trace := range traces {
		out = appendTraceV04(out, trace)
	}
	return out
}

func appendTraceV04(out []byte, trace []fixtureSpan) []byte {
	out = msgp.AppendArrayHeader(out, uint32(len(trace)))
	for _, span := range trace {
		out = appendSpanV04(out, span)
	}
	return out
}

// encodeV05 writes the traces first, collecting the string table, and then puts the table
// in front. As in the tracers, index 0 of the table is the empty string.
func encodeV05(traces [][]fixtureSpan) []byte {
	table := stringTable{index: map[string]uint32{}}
	table.ref("")
	body := msgp.AppendArrayHeader(nil, uint32(len(traces)))
	for _, trace := range traces {
		body = msgp.AppendArrayHeader(body, uint32(len(trace)))
		for _, s := range trace {
			body = msgp.AppendArrayHeader(body, 12)
			body = msgp.AppendUint32(body, table.ref(s.Service))
			body = msgp.AppendUint32(body, table.ref(s.Name))
			body = msgp.AppendUint32(body, table.ref(s.Resource))
			body = msgp.AppendUint64(body, s.TraceID)
			body = msgp.AppendUint64(body, s.SpanID)
			body = msgp.AppendUint64(body, s.ParentID)
			body = msgp.AppendInt64(body, s.Start)
			body = msgp.AppendInt64(body, s.Duration)
			body = msgp.AppendInt32(body, s.Error)
			body = msgp.AppendMapHeader(body, uint32(len(s.Meta)))
			for _, key := range slices.Sorted(maps.Keys(s.Meta)) {
				body = msgp.AppendUint32(body, table.ref(key))
				body = msgp.AppendUint32(body, table.ref(s.Meta[key]))
			}
			body = msgp.AppendMapHeader(body, uint32(len(s.Metrics)))
			for _, key := range slices.Sorted(maps.Keys(s.Metrics)) {
				body = msgp.AppendUint32(body, table.ref(key))
				body = msgp.AppendFloat64(body, s.Metrics[key])
			}
			body = msgp.AppendUint32(body, table.ref(s.Type))
		}
	}
	out := msgp.AppendArrayHeader(nil, 2)
	out = msgp.AppendArrayHeader(out, uint32(len(table.strings)))
	for _, str := range table.strings {
		out = msgp.AppendString(out, str)
	}
	return append(out, body...)
}

// stringTable assigns each distinct string an index, in order of first use.
type stringTable struct {
	strings []string
	index   map[string]uint32
}

func (t *stringTable) ref(s string) uint32 {
	if i, ok := t.index[s]; ok {
		return i
	}
	i := uint32(len(t.strings))
	t.strings = append(t.strings, s)
	t.index[s] = i
	return i
}

// encodeV07 writes a tracer payload with each trace as one chunk with priority -128, which
// means no sampling priority, as the agent sets for v04 and v05 traces.
func encodeV07(tracer fixtureTracerPayload, traces [][]fixtureSpan) []byte {
	out := msgp.AppendMapHeader(nil, 8)
	for _, field := range []struct{ key, value string }{
		{"language_name", tracer.LanguageName},
		{"language_version", tracer.LanguageVersion},
		{"tracer_version", tracer.TracerVersion},
		{"runtime_id", tracer.RuntimeID},
		{"env", tracer.Env},
		{"hostname", tracer.Hostname},
		{"app_version", tracer.AppVersion},
	} {
		out = msgp.AppendString(out, field.key)
		out = msgp.AppendString(out, field.value)
	}
	out = msgp.AppendString(out, "chunks")
	out = msgp.AppendArrayHeader(out, uint32(len(traces)))
	for _, trace := range traces {
		out = msgp.AppendMapHeader(out, 2)
		out = msgp.AppendString(out, "priority")
		out = msgp.AppendInt32(out, -128)
		out = msgp.AppendString(out, "spans")
		out = appendTraceV04(out, trace)
	}
	return out
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
