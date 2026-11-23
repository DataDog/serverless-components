package workload

import (
	"bytes"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"slices"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/events"
	"github.com/tinylib/msgp/msgp"
)

func TestSpanFixtureEncodesV04(t *testing.T) {
	payload, traceCount, err := encodeSpanFixture("two-traces-v04")
	if err != nil {
		t.Fatal(err)
	}
	if traceCount != 2 {
		t.Fatalf("trace count %d, want 2", traceCount)
	}
	size, remaining, err := msgp.ReadArrayHeaderBytes(payload)
	if err != nil || size != 2 {
		t.Fatalf("outer array size %d, error %v, want 2", size, err)
	}
	spans, _, err := msgp.ReadArrayHeaderBytes(remaining)
	if err != nil || spans != 2 {
		t.Fatalf("first trace has %d spans, error %v, want 2", spans, err)
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
