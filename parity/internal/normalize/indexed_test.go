package normalize

import (
	"encoding/json"
	"os"
	"reflect"
	"slices"
	"strings"
	"testing"

	pb "github.com/DataDog/datadog-agent/pkg/proto/pbgo/trace"
	"github.com/DataDog/datadog-agent/pkg/proto/pbgo/trace/idx"
	"github.com/DataDog/datadog-agent/test/fakeintake/aggregator"
	"github.com/DataDog/datadog-agent/test/fakeintake/api"

	"github.com/DataDog/serverless-components/parity/internal/capture"
)

// indexedCapture returns the trace payload that Go serverless-init 1.12.0 sent for the
// internal-spans, sampling-priorities and two-traces fixtures, with DD_ENV=parity-env and
// DD_VERSION=1.0.0. Go sends every trace in the indexed format.
func indexedCapture(t *testing.T) capture.Capture {
	t.Helper()
	data, err := os.ReadFile("testdata/indexed-traces.json")
	if err != nil {
		t.Fatal(err)
	}
	var file struct{ Payloads []api.Payload }
	if err := json.Unmarshal(data, &file); err != nil {
		t.Fatal(err)
	}
	decoded, err := aggregator.ParseTracePayload(file.Payloads[0])
	if err != nil {
		t.Fatal(err)
	}
	return capture.Capture{Payloads: []capture.Payload{{Route: "/api/v0.2/traces", ArrivedAt: file.Payloads[0].Timestamp, Decoded: decoded}}}
}

func TestIndexedTracesBecomeSpans(t *testing.T) {
	got, err := Apply(indexedCapture(t), nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Undecoded) != 0 {
		t.Fatalf("undecoded %+v, want none", got.Undecoded)
	}
	var chunks []Chunk
	for _, payload := range got.Traces {
		if payload.Env != "parity-env" || payload.AgentVersion != "7.85.2" {
			t.Errorf("agent payload env %q version %q, want parity-env and 7.85.2", payload.Env, payload.AgentVersion)
		}
		for _, tracer := range payload.TracerPayloads {
			chunks = append(chunks, tracer.Chunks...)
		}
	}
	if len(chunks) != 10 {
		t.Fatalf("got %d chunks, want 10", len(chunks))
	}
	var kept Chunk
	for _, c := range chunks {
		if c.Spans[0].Resource == "GET /kept" {
			kept = c
		}
	}
	if kept.Priority != 1 || kept.Origin != "local" || len(kept.Spans) != 2 {
		t.Fatalf("unexpected GET /kept chunk %+v", kept)
	}
	root, client := kept.Spans[0], kept.Spans[1]
	if client.Resource != "GET https://example.com/" {
		root, client = client, root
	}
	wantClient := Span{
		Service: "parity-app", Name: "parity.client", Resource: "GET https://example.com/", Type: "http",
		TraceID: "7005", SpanID: "8010", ParentID: "8009", Start: 1700000004001000000, Duration: 1000000,
		Meta: map[string]string{
			"_dd.convertedv1": "v04", "_dd.datadog_init_version": client.Meta["_dd.datadog_init_version"], "_dd.origin": "local",
			"env": "parity-env", "version": "1.0.0", "http.url": "https://example.com/", "instance": client.Meta["instance"],
			"local": "true", "service": "parity-app", "span.kind": "client",
		},
	}
	if !reflect.DeepEqual(client, wantClient) {
		t.Errorf("client span\n got %+v\nwant %+v", client, wantClient)
	}
	if root.ParentID != "" || root.Metrics["_top_level"] != 1 || root.Metrics["_sampling_priority_v1"] != 1 || root.Meta["http.status_code"] != "200" {
		t.Errorf("unexpected root span %+v", root)
	}
	var errored Span
	for _, c := range chunks {
		if c.Spans[0].Resource == "GET /priority-0-error" {
			errored = c.Spans[0]
		}
	}
	if errored.Error != 1 || errored.Meta["error.message"] != "parity failure" {
		t.Errorf("unexpected errored span %+v", errored)
	}

	timing, err := timingInput(indexedCapture(t))
	if err != nil {
		t.Fatal(err)
	}
	if len(timing.Arrivals) != 1 || !slices.Contains(timing.Arrivals[0].Items, "span:GET https://example.com/") || len(timing.Arrivals[0].Items) != 13 {
		t.Errorf("arrivals %+v, want one with the 13 resources Go kept", timing.Arrivals)
	}
}

// indexedPayload returns an agent payload with one indexed span whose attributes are attrs, with
// strings as the string table.
func indexedPayload(strings []string, attrs map[uint32]*idx.AnyValue, traceID []byte) capture.Payload {
	return capture.Payload{Route: "/api/v0.2/traces", Decoded: []*aggregator.TracePayload{{AgentPayload: &pb.AgentPayload{
		IdxTracerPayloads: []*idx.TracerPayload{{Strings: strings, Chunks: []*idx.TraceChunk{{
			TraceID: traceID, Spans: []*idx.Span{{ServiceRef: 1, SpanID: 2, Attributes: attrs}},
		}}}},
	}}}}
}

func TestIndexedConversion(t *testing.T) {
	traceID := []byte{0, 0, 0, 0, 0, 0, 0, 0xab, 0, 0, 0, 0, 0, 0, 0, 7}
	for _, tc := range []struct {
		name      string
		strings   []string
		attrs     map[uint32]*idx.AnyValue
		traceID   []byte
		wantSpan  Span
		wantError string
	}{
		{
			name:    "attribute types and the high trace ID bits",
			strings: []string{"", "svc", "s", "v", "b", "i", "d", "raw"},
			attrs: map[uint32]*idx.AnyValue{
				2: {Value: &idx.AnyValue_StringValueRef{StringValueRef: 3}},
				4: {Value: &idx.AnyValue_BoolValue{BoolValue: true}},
				5: {Value: &idx.AnyValue_IntValue{IntValue: 4}},
				6: {Value: &idx.AnyValue_DoubleValue{DoubleValue: 0.5}},
				7: {Value: &idx.AnyValue_BytesValue{BytesValue: []byte{1, 2}}},
			},
			traceID: traceID,
			wantSpan: Span{Service: "svc", TraceID: "7", SpanID: "2",
				Meta:       map[string]string{"s": "v", "b": "true", "_dd.p.tid": "00000000000000ab"},
				Metrics:    map[string]float64{"i": 4, "d": 0.5},
				MetaStruct: map[string]string{"raw": "AQI="}},
		},
		{
			name:      "an array attribute has no legacy form",
			strings:   []string{"", "svc", "list"},
			attrs:     map[uint32]*idx.AnyValue{2: {Value: &idx.AnyValue_ArrayValue{ArrayValue: &idx.ArrayValue{}}}},
			traceID:   traceID,
			wantError: `indexed span svc: attribute "list" has an array value, which a legacy span cannot hold`,
		},
		{
			name:      "a reference outside the string table",
			strings:   []string{"", "svc"},
			attrs:     map[uint32]*idx.AnyValue{9: {Value: &idx.AnyValue_DoubleValue{DoubleValue: 1}}},
			traceID:   traceID,
			wantError: "string reference 9 is outside the string table of 2 strings",
		},
		{
			name:      "a trace ID that is not 16 bytes",
			strings:   []string{"", "svc"},
			traceID:   []byte{1},
			wantError: "trace ID of 1 bytes, want 16",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got, err := Apply(capture.Capture{Payloads: []capture.Payload{indexedPayload(tc.strings, tc.attrs, tc.traceID)}}, nil)
			if err != nil {
				t.Fatal(err)
			}
			if tc.wantError != "" {
				if len(got.Undecoded) != 1 || !strings.Contains(got.Undecoded[0].Error, tc.wantError) {
					t.Fatalf("undecoded %+v, want one containing %q", got.Undecoded, tc.wantError)
				}
				if chunks := got.Traces[0].TracerPayloads; len(chunks) != 0 {
					t.Fatalf("a payload that failed to convert is also in the view: %+v", chunks)
				}
				return
			}
			if len(got.Undecoded) != 0 {
				t.Fatalf("unexpected undecoded %+v", got.Undecoded)
			}
			span := got.Traces[0].TracerPayloads[0].Chunks[0].Spans[0]
			if !reflect.DeepEqual(span, tc.wantSpan) {
				t.Fatalf("got %+v\nwant %+v", span, tc.wantSpan)
			}
		})
	}
}
