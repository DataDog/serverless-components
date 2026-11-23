package workload

import (
	"bytes"
	"io"
	"maps"
	"net"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"reflect"
	"slices"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	pb "github.com/DataDog/datadog-agent/pkg/proto/pbgo/trace"
	"github.com/DataDog/datadog-agent/pkg/proto/pbgo/trace/idx"
	"github.com/DataDog/serverless-components/parity/internal/events"
	"github.com/tinylib/msgp/msgp"
	"google.golang.org/protobuf/proto"
)

func TestSpanFixtureEncodesV04(t *testing.T) {
	encoded, err := encodeSpanFixture("two-traces-v04")
	if err != nil {
		t.Fatal(err)
	}
	if encoded.path != "/v0.4/traces" || encoded.traceCount != 2 {
		t.Fatalf("path %q, trace count %d, want /v0.4/traces and 2", encoded.path, encoded.traceCount)
	}
	size, remaining, err := msgp.ReadArrayHeaderBytes(encoded.body)
	if err != nil || size != 2 {
		t.Fatalf("outer array size %d, error %v, want 2", size, err)
	}
	spans, _, err := msgp.ReadArrayHeaderBytes(remaining)
	if err != nil || spans != 2 {
		t.Fatalf("first trace has %d spans, error %v, want 2", spans, err)
	}
}

// decodedSpan holds the span fields that both the v0.5 and the v0.7 decoders produce, so the
// two payloads can be compared with the same expected values.
type decodedSpan struct {
	service, name, resource, spanType string
	traceID, spanID, parentID         uint64
	start, duration                   int64
	isError                           bool
	meta                              map[string]string
	metrics                           map[string]float64
}

type decodedChunk struct {
	priority int32
	spans    []decodedSpan
}

// decodeV05 decodes body with the decoder serverless-init 1.12.0 uses for /v0.5/traces.
func decodeV05(t *testing.T, body []byte) []decodedChunk {
	t.Helper()
	var payload idx.InternalTracerPayload
	if err := payload.UnmarshalMsgDictionary(body); err != nil {
		t.Fatal(err)
	}
	var chunks []decodedChunk
	for _, chunk := range payload.Chunks {
		decoded := decodedChunk{priority: chunk.Priority}
		for _, span := range chunk.Spans {
			d := decodedSpan{
				service: span.Service(), name: span.Name(), resource: span.Resource(), spanType: span.Type(),
				traceID: chunk.LegacyTraceID(), spanID: span.SpanID(), parentID: span.ParentID(),
				start: int64(span.Start()), duration: int64(span.Duration()), isError: span.Error(),
				meta: map[string]string{}, metrics: map[string]float64{},
			}
			for key, value := range span.Attributes() {
				if double, ok := value.Value.(*idx.AnyValue_DoubleValue); ok {
					d.metrics[payload.Strings.Get(key)] = double.DoubleValue
				} else {
					d.meta[payload.Strings.Get(key)] = value.AsString(payload.Strings)
				}
			}
			decoded.spans = append(decoded.spans, d)
		}
		chunks = append(chunks, decoded)
	}
	return chunks
}

// decodeV07 decodes body with the decoder the Go trace agent uses for /v0.7/traces.
func decodeV07(t *testing.T, body []byte) (*pb.TracerPayload, []decodedChunk) {
	t.Helper()
	var payload pb.TracerPayload
	remaining, err := payload.UnmarshalMsg(body)
	if err != nil || len(remaining) != 0 {
		t.Fatalf("error %v, %d bytes left over", err, len(remaining))
	}
	var chunks []decodedChunk
	for _, chunk := range payload.Chunks {
		decoded := decodedChunk{priority: chunk.Priority}
		for _, span := range chunk.Spans {
			decoded.spans = append(decoded.spans, decodedSpan{
				service: span.Service, name: span.Name, resource: span.Resource, spanType: span.Type,
				traceID: span.TraceID, spanID: span.SpanID, parentID: span.ParentID,
				start: span.Start, duration: span.Duration, isError: span.Error != 0,
				meta: nonNil(span.Meta), metrics: nonNil(span.Metrics),
			})
		}
		chunks = append(chunks, decoded)
	}
	return &payload, chunks
}

func nonNil[V any](m map[string]V) map[string]V {
	if m == nil {
		return map[string]V{}
	}
	return m
}

// twoTracesWant returns the chunks of a two-traces fixture whose IDs start at traceID and
// spanID and whose resources carry version. The first span of the v0.5 payload also gets the
// _dd.convertedv1 tag from the Go decoder, which the caller adds.
func twoTracesWant(version string, traceID, spanID uint64) []decodedChunk {
	return []decodedChunk{
		{priority: -128, spans: []decodedSpan{
			{service: "parity-app", name: "parity.request", resource: "GET /" + version, spanType: "web",
				traceID: traceID, spanID: spanID, start: 1700000000000000000, duration: 50000000,
				meta:    map[string]string{"http.status_code": "200", "span.kind": "server"},
				metrics: map[string]float64{"_top_level": 1}},
			{service: "parity-app", name: "parity.query", resource: "SELECT name FROM " + version + "_users", spanType: "sql",
				traceID: traceID, spanID: spanID + 1, parentID: spanID, start: 1700000000010000000, duration: 20000000,
				meta: map[string]string{}, metrics: map[string]float64{}},
		}},
		{priority: -128, spans: []decodedSpan{
			{service: "parity-app", name: "parity.request", resource: "GET /" + version + "/fail", spanType: "web",
				traceID: traceID + 1, spanID: spanID + 2, start: 1700000001000000000, duration: 80000000, isError: true,
				meta:    map[string]string{"http.status_code": "500", "span.kind": "server"},
				metrics: map[string]float64{"_top_level": 1}},
			{service: "parity-app", name: "parity.cache", resource: "GET " + version + "-session", spanType: "cache",
				traceID: traceID + 1, spanID: spanID + 3, parentID: spanID + 2, start: 1700000001005000000, duration: 10000000,
				meta: map[string]string{}, metrics: map[string]float64{}},
		}},
	}
}

var pythonTracerHeaders = map[string]string{
	"Datadog-Meta-Lang":             "python",
	"Datadog-Meta-Lang-Version":     "3.12.7",
	"Datadog-Meta-Lang-Interpreter": "CPython",
	"Datadog-Meta-Tracer-Version":   "2.18.1",
	"Datadog-Container-ID":          "9d3bc1f2e8a74b6d5c0f1e2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e",
}

func TestSpanFixtureEncodesV05(t *testing.T) {
	encoded, err := encodeSpanFixture("two-traces-v05")
	if err != nil {
		t.Fatal(err)
	}
	if encoded.path != "/v0.5/traces" || encoded.traceCount != 2 || !maps.Equal(encoded.headers, pythonTracerHeaders) {
		t.Fatalf("path %q, trace count %d, headers %v", encoded.path, encoded.traceCount, encoded.headers)
	}
	// Tracers put the empty string at index 0 of the string table.
	_, remaining, err := msgp.ReadArrayHeaderBytes(encoded.body)
	if err == nil {
		_, remaining, err = msgp.ReadArrayHeaderBytes(remaining)
	}
	if first, _, err2 := msgp.ReadStringBytes(remaining); err != nil || err2 != nil || first != "" {
		t.Fatalf("first string table entry %q, errors %v and %v, want the empty string", first, err, err2)
	}
	want := twoTracesWant("v05", 1051, 2051)
	want[0].spans[0].meta["_dd.convertedv1"] = "v05"
	if got := decodeV05(t, encoded.body); !reflect.DeepEqual(got, want) {
		t.Fatalf("decoded\n %+v\nwant\n %+v", got, want)
	}
}

func TestSpanFixtureEncodesV07(t *testing.T) {
	encoded, err := encodeSpanFixture("two-traces-v07")
	if err != nil {
		t.Fatal(err)
	}
	if encoded.path != "/v0.7/traces" || encoded.traceCount != 2 || !maps.Equal(encoded.headers, pythonTracerHeaders) {
		t.Fatalf("path %q, trace count %d, headers %v", encoded.path, encoded.traceCount, encoded.headers)
	}
	payload, got := decodeV07(t, encoded.body)
	wantPayload := &pb.TracerPayload{
		LanguageName: "python", LanguageVersion: "3.13.1", TracerVersion: "3.1.0",
		RuntimeID: "4f6c2b8e-1d3a-4e5f-9a7b-0c1d2e3f4a5b", Env: "parity-tracer-env",
		Hostname: "parity-tracer-host", AppVersion: "1.4.2",
	}
	payload.Chunks = nil
	if !proto.Equal(payload, wantPayload) {
		t.Fatalf("tracer payload fields %v, want %v", payload, wantPayload)
	}
	if want := twoTracesWant("v07", 1071, 2071); !reflect.DeepEqual(got, want) {
		t.Fatalf("decoded\n %+v\nwant\n %+v", got, want)
	}
}

func TestRealSendersSpansPostsEachVersionToItsEndpoint(t *testing.T) {
	for fixture, path := range map[string]string{
		"two-traces-v04": "/v0.4/traces",
		"two-traces-v05": "/v0.5/traces",
		"two-traces-v07": "/v0.7/traces",
	} {
		t.Run(fixture, func(t *testing.T) {
			var gotPath string
			var header http.Header
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				if r.URL.Path != "/info" {
					gotPath, header = r.URL.Path, r.Header
				}
			}))
			defer server.Close()
			senders := &RealSenders{TraceURL: server.URL}
			if _, err := senders.Spans(fixture); err != nil {
				t.Fatal(err)
			}
			if gotPath != path || header.Get("Content-Type") != "application/msgpack" || header.Get("X-Datadog-Trace-Count") != "2" {
				t.Fatalf("path %q, headers %v, want %s, application/msgpack and 2 traces", gotPath, header, path)
			}
			wantLang := "python"
			if fixture == "two-traces-v04" {
				wantLang = ""
			}
			if got := header.Get("Datadog-Meta-Lang"); got != wantLang {
				t.Fatalf("Datadog-Meta-Lang %q, want %q", got, wantLang)
			}
		})
	}
}

func TestRealSendersSpansWaitsForReceiverAndPosts(t *testing.T) {
	var infoCalls atomic.Int32
	var mu sync.Mutex
	var contentType, traceCount string
	var body []byte
	var lastInfo time.Time
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/info":
			mu.Lock()
			lastInfo = time.Now()
			mu.Unlock()
			if infoCalls.Add(1) < 3 {
				w.WriteHeader(http.StatusServiceUnavailable)
			}
		case "/v0.4/traces":
			mu.Lock()
			defer mu.Unlock()
			contentType, traceCount = r.Header.Get("Content-Type"), r.Header.Get("X-Datadog-Trace-Count")
			body, _ = io.ReadAll(r.Body)
		}
	}))
	defer server.Close()

	var readyCalls int
	var sentAt time.Time
	senders := &RealSenders{TraceURL: server.URL, PollInterval: 1, OnReceiverReady: func() { readyCalls++ }}
	for range 2 {
		var err error
		if sentAt, err = senders.Spans("two-traces-v04"); err != nil {
			t.Fatal(err)
		}
	}
	if infoCalls.Load() != 3 || readyCalls != 1 {
		t.Fatalf("info calls %d, ready calls %d, want 3 and 1", infoCalls.Load(), readyCalls)
	}
	mu.Lock()
	defer mu.Unlock()
	if sentAt.Before(lastInfo) {
		t.Fatalf("send moment %v is before the last /info answer at %v", sentAt, lastInfo)
	}
	if contentType != "application/msgpack" || traceCount != "2" || len(body) == 0 {
		t.Fatalf("content type %q, trace count %q, body length %d", contentType, traceCount, len(body))
	}
}

func TestRealSendersDogStatsDWritesOneDatagram(t *testing.T) {
	conn, err := net.ListenPacket("udp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	senders := &RealSenders{DogStatsDAddress: conn.LocalAddr().String()}
	if _, err := senders.DogStatsD([]string{"a:1|c", "b:2|g"}); err != nil {
		t.Fatal(err)
	}
	buffer := make([]byte, 1024)
	n, _, err := conn.ReadFrom(buffer)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(buffer[:n]), "a:1|c\nb:2|g"; got != want {
		t.Fatalf("datagram %q, want %q", got, want)
	}
}

func TestNewRealSendersLogsReceiverReady(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))
	defer server.Close()
	log := filepath.Join(t.TempDir(), "events.jsonl")
	var errOut bytes.Buffer
	senders := NewRealSenders(log, &errOut)
	senders.TraceURL = server.URL
	if _, err := senders.Spans("two-traces-v04"); err != nil {
		t.Fatal(err)
	}
	got, err := events.Read(log)
	if err != nil || len(got) != 1 || got[0].Kind != "receiver_ready" {
		t.Fatalf("events %+v, error %v, want one receiver_ready", got, err)
	}
}

func TestNewRealSendersReportsEventLogFailure(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))
	defer server.Close()
	var errOut bytes.Buffer
	senders := NewRealSenders(filepath.Join(t.TempDir(), "missing", "events.jsonl"), &errOut)
	senders.TraceURL = server.URL
	if _, err := senders.Spans("two-traces-v04"); err != nil {
		t.Fatal(err)
	}
	if !bytes.Contains(errOut.Bytes(), []byte(`parity-child: cannot write event "receiver_ready"`)) {
		t.Fatalf("stderr %q, want the event write failure", errOut.String())
	}
}

func TestFixtureResources(t *testing.T) {
	got, err := FixtureResources("two-traces-v04")
	if err != nil {
		t.Fatal(err)
	}
	want := []string{"GET /", "SELECT 1", "GET /fail", "GET key"}
	if !slices.Equal(got, want) {
		t.Fatalf("got %q, want %q", got, want)
	}
	if _, err := FixtureResources("missing"); err == nil {
		t.Fatal("missing fixture: got no error")
	}
}

// A listener that never answers must not hold the workload past the run's deadline.
func TestRealSendersHTTPGetTimesOut(t *testing.T) {
	release := make(chan struct{})
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { <-release }))
	defer server.Close()
	defer close(release)
	senders := &RealSenders{HTTPTimeout: 100 * time.Millisecond}
	start := time.Now()
	if _, _, err := senders.HTTPGet(server.URL + "/live"); err == nil {
		t.Fatal("HTTPGet succeeded against a server that never answers")
	}
	if elapsed := time.Since(start); elapsed > 2*time.Second {
		t.Fatalf("HTTPGet took %v, want about the 100-millisecond timeout", elapsed)
	}
}
