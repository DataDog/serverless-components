package normalize

import (
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"

	metricspb "github.com/DataDog/agent-payload/v5/gogen"
	pb "github.com/DataDog/datadog-agent/pkg/proto/pbgo/trace"
	"github.com/DataDog/datadog-agent/test/fakeintake/aggregator"
	"github.com/DataDog/sketches-go/ddsketch"
	"google.golang.org/protobuf/proto"

	"github.com/DataDog/serverless-components/parity/internal/capture"
	"github.com/DataDog/serverless-components/parity/internal/events"
	"github.com/DataDog/serverless-components/parity/internal/runner"
	"github.com/DataDog/serverless-components/parity/internal/timing"
)

func writeFile(t *testing.T, content string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "normalize.yaml")
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	return path
}

func seriesCapture(tags ...string) capture.Capture {
	return capture.Capture{Payloads: []capture.Payload{{
		Route: "/api/v2/series",
		Decoded: []*aggregator.MetricSeries{{MetricPayload_MetricSeries: metricspb.MetricPayload_MetricSeries{
			Metric: "parity.requests", Type: metricspb.MetricPayload_COUNT, Tags: tags,
			Points: []*metricspb.MetricPayload_MetricPoint{{Value: 1, Timestamp: 1700000000}},
		}}},
	}}}
}

func TestLoadRulesRejectsMissingReason(t *testing.T) {
	path := writeFile(t, "rules:\n  - action: drop\n    path: logs[].hostname\n")
	_, _, err := LoadRules(path)
	if err == nil || !strings.Contains(err.Error(), "reason") {
		t.Fatalf("got error %v, want an error about the missing reason", err)
	}
}

func TestLoadRulesRejectsInvalidRules(t *testing.T) {
	for _, tc := range []struct{ name, rule string }{
		{"unknown action", "action: rename\n    path: logs[].hostname"},
		{"unknown area", "action: drop\n    path: timing.exited"},
		{"empty path", "action: drop\n    path: \"\""},
		{"unclosed bracket", "action: drop\n    path: logs[.hostname"},
		{"a filter without a value", "action: drop\n    path: series[metric=].host"},
		{"when_matches on another action", "action: drop\n    path: logs[].hostname\n    when_matches: x"},
		{"invalid when_matches", "action: mask\n    path: logs[].hostname\n    when_matches: \"[\""},
		{"relative without a unit", "action: relative\n    path: logs[].timestamp"},
		{"relative with an unknown unit", "action: relative\n    path: logs[].timestamp\n    relative: {unit: us}"},
		{"relative alignment finer than the unit", "action: relative\n    path: logs[].timestamp\n    relative: {unit: s, align: 500ms}"},
		{"relative settings on another action", "action: mask\n    path: logs[].timestamp\n    relative: {unit: ms}"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			path := writeFile(t, "rules:\n  - "+tc.rule+"\n    reason: test\n")
			if _, _, err := LoadRules(path); err == nil {
				t.Fatal("got no error")
			}
		})
	}
}

func TestLoadRulesTolerances(t *testing.T) {
	for _, tc := range []struct {
		name, content string
		want          timing.Tolerances
	}{
		{"defaults", "rules: []\n", timing.Tolerances{Interval: time.Second, Delay: time.Second, Shutdown: time.Second, Percentile: 0.01, FlushGap: 500 * time.Millisecond}},
		{"override", "tolerances:\n  delay: 2500ms\n  percentile: 0.02\n  flush_gap: 200ms\n  reason: test\nrules: []\n",
			timing.Tolerances{Interval: time.Second, Delay: 2500 * time.Millisecond, Shutdown: time.Second, Percentile: 0.02, FlushGap: 200 * time.Millisecond}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			_, got, err := LoadRules(writeFile(t, tc.content))
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("got %+v, want %+v", got, tc.want)
			}
		})
	}
	for _, content := range []string{
		"tolerances:\n  delay: 2s\nrules: []\n",
		"tolerances:\n  flush_gap: 2s\nrules: []\n",
		"tolerances:\n  flush_gap: 0s\n  reason: test\nrules: []\n",
		"tolerances:\n  flush_gap: -1s\n  reason: test\nrules: []\n",
	} {
		if _, _, err := LoadRules(writeFile(t, content)); err == nil {
			t.Fatalf("invalid tolerances %q: got no error", content)
		}
	}
}

func TestRelabelPreservesParentChild(t *testing.T) {
	c := capture.Capture{Payloads: []capture.Payload{{
		Route: "/api/v0.2/traces",
		Decoded: []*aggregator.TracePayload{{AgentPayload: &pb.AgentPayload{TracerPayloads: []*pb.TracerPayload{{
			Chunks: []*pb.TraceChunk{{Spans: []*pb.Span{
				{Resource: "GET /", TraceID: 5, SpanID: 99},
				{Resource: "SELECT 1", TraceID: 5, SpanID: 7, ParentID: 99},
			}}},
		}}}}},
	}}}
	rules := []Rule{
		{Action: "relabel", Path: "traces[].tracer_payloads[].chunks[].spans[].trace_id", Reason: "test"},
		{Action: "relabel", Path: "traces[].tracer_payloads[].chunks[].spans[].span_id", Reason: "test"},
		{Action: "relabel", Path: "traces[].tracer_payloads[].chunks[].spans[].parent_id", Label: "span", Reason: "test"},
	}
	got, err := Apply(c, rules)
	if err != nil {
		t.Fatal(err)
	}
	want := []Span{
		{Resource: "GET /", TraceID: "trace-1", SpanID: "span-1"},
		{Resource: "SELECT 1", TraceID: "trace-1", SpanID: "span-2", ParentID: "span-1"},
	}
	if spans := got.Traces[0].TracerPayloads[0].Chunks[0].Spans; !reflect.DeepEqual(spans, want) {
		t.Fatalf("got %+v, want %+v", spans, want)
	}
}

func TestPlaceholderReplacesVersionTag(t *testing.T) {
	for _, tc := range []struct {
		name, path string
		tags, want []string
	}{
		{"tag with the key", "series[].tags[datadog_init_version]",
			[]string{"env:prod", "datadog_init_version:1.2.3"}, []string{"env:prod", "datadog_init_version:<placeholder>"}},
		{"key containing a dot", "series[].tags[_dd.datadog_init_version]",
			[]string{"_dd.datadog_init_version:1.2.3"}, []string{"_dd.datadog_init_version:<placeholder>"}},
		{"no tag with the key", "series[].tags[datadog_init_version]",
			[]string{"datadog_init_version_extra:1"}, []string{"datadog_init_version_extra:1"}},
		{"bare tag without a value", "series[].tags[datadog_init_version]",
			[]string{"datadog_init_version"}, []string{"datadog_init_version"}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got, err := Apply(seriesCapture(tc.tags...), []Rule{{Action: "placeholder", Path: tc.path, Reason: "test"}})
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(got.Series[0].Tags, tc.want) {
				t.Fatalf("got %q, want %q", got.Series[0].Tags, tc.want)
			}
		})
	}
}

func TestPlaceholderReplacesMapValue(t *testing.T) {
	c := capture.Capture{Payloads: []capture.Payload{{
		Route: "/api/v0.2/traces",
		Decoded: []*aggregator.TracePayload{{AgentPayload: &pb.AgentPayload{TracerPayloads: []*pb.TracerPayload{{
			Chunks: []*pb.TraceChunk{{Spans: []*pb.Span{{SpanID: 1, Meta: map[string]string{"_dd.datadog_init_version": "1.2.3", "env": "prod"}}}}},
		}}}}},
	}}}
	got, err := Apply(c, []Rule{{Action: "placeholder", Path: "traces[].tracer_payloads[].chunks[].spans[].meta[_dd.datadog_init_version]", Reason: "test"}})
	if err != nil {
		t.Fatal(err)
	}
	want := map[string]string{"_dd.datadog_init_version": "<placeholder>", "env": "prod"}
	if meta := got.Traces[0].TracerPayloads[0].Chunks[0].Spans[0].Meta; !reflect.DeepEqual(meta, want) {
		t.Fatalf("got %v, want %v", meta, want)
	}
}

func TestSortTags(t *testing.T) {
	got, err := Apply(seriesCapture("z:1", "a:2", "m:3"), []Rule{{Action: "sort", Path: "series[].tags", Reason: "test"}})
	if err != nil {
		t.Fatal(err)
	}
	if want := []string{"a:2", "m:3", "z:1"}; !reflect.DeepEqual(got.Series[0].Tags, want) {
		t.Fatalf("got %q, want %q", got.Series[0].Tags, want)
	}
}

func TestProcessViewKeepsOnlyChildMarkerLines(t *testing.T) {
	c := capture.Capture{
		Outcome: runner.Outcome{
			ExitCode: 7,
			Stdout:   "agent log line\nparity-child: started\r\nparity-child: got SIGTERM\n",
			Stderr:   "2026-11-22 ERROR agent\nparity-child: warning\nnot parity-child: inside\n",
		},
		Events: []events.Event{
			{Kind: "started", Detail: map[string]any{
				"arguments":   []any{"/parity-workload", "run"},
				"environment": map[string]any{"DD_API_KEY": "fake", "DD_SERVICE": "parity-app", "PATH": "/bin", "K_SERVICE": "x"},
			}},
			{Kind: "signal", Detail: map[string]any{"signal": "SIGTERM"}},
			{Kind: "printed", Detail: map[string]any{"line": "parity-child: started"}},
			{Kind: "signal", Detail: map[string]any{"signal": "SIGINT"}},
		},
	}
	got, err := Apply(c, nil)
	if err != nil {
		t.Fatal(err)
	}
	want := ProcessView{
		ExitCode:    7,
		Arguments:   []string{"/parity-workload", "run"},
		Signals:     []string{"SIGTERM", "SIGINT"},
		Environment: map[string]string{"DD_API_KEY": "fake", "DD_SERVICE": "parity-app"},
		Stdout:      []string{"parity-child: started", "parity-child: got SIGTERM"},
		Stderr:      []string{"parity-child: warning"},
	}
	if !reflect.DeepEqual(got.Process, want) {
		t.Fatalf("got %+v, want %+v", got.Process, want)
	}
}

func TestSentItems(t *testing.T) {
	for _, tc := range []struct {
		name   string
		kind   string
		detail map[string]any
		want   []string
	}{
		{"dogstatsd lines", "sent", map[string]any{"step": "send_dogstatsd", "lines": []any{"parity.requests:1|c|#route:/", "parity.latency:12|d", "parity.requests:2|c", "_sc|check|0", "_e{1,1}:a|b"}},
			[]string{"metric:parity.requests|route:/", "metric:parity.latency", "metric:parity.requests"}},
		{"dogstatsd tags are sorted after the sample rate", "sent", map[string]any{"step": "send_dogstatsd", "lines": []any{"m:1|c|@0.5|#b:2,a:1,b:2|T1700000000"}},
			[]string{"metric:m|a:1,b:2"}},
		{"span fixture", "sent", map[string]any{"step": "send_spans", "fixture": "two-traces-v04"},
			[]string{"span:GET /", "span:SELECT 1", "span:GET /fail", "span:GET key"}},
		{"appended lines are sent as written", "sent", map[string]any{"step": "append_log", "lines": []any{"plain line", "parity-child: marked"}},
			[]string{"log:plain line", "log:parity-child: marked"}},
		{"printed line keeps the marker", "printed", map[string]any{"stream": "stdout", "line": "parity-child: started"},
			[]string{"log:parity-child: started"}},
		{"http request sends nothing", "sent", map[string]any{"step": "http_get", "url": "http://x", "status": 200}, nil},
		{"failed send", "sent", map[string]any{"step": "send_dogstatsd", "lines": []any{"a:1|c"}, "error": "refused"}, nil},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got, err := sentItems(events.Event{Kind: tc.kind, Detail: tc.detail})
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("got %q, want %q", got, tc.want)
			}
		})
	}
}

func TestTimingInput(t *testing.T) {
	base := time.Date(2026, 11, 22, 10, 0, 0, 0, time.UTC)
	c := capture.Capture{
		Payloads: []capture.Payload{
			{Route: "/api/v2/logs", ArrivedAt: base.Add(4 * time.Second), Decoded: []*aggregator.Log{{Message: "parity-child: started"}, {Message: "parity-child: started"}}},
			{Route: "/api/v0.2/stats", ArrivedAt: base.Add(5 * time.Second), Decoded: []*aggregator.APMStatsPayload{{StatsPayload: &pb.StatsPayload{Stats: []*pb.ClientStatsPayload{{
				Stats: []*pb.ClientStatsBucket{{Stats: []*pb.ClientGroupedStats{{Resource: "GET /"}}}},
			}}}}}},
			{Route: "/api/v2/series", ArrivedAt: base.Add(3 * time.Second), Decoded: []*aggregator.MetricSeries{{MetricPayload_MetricSeries: metricspb.MetricPayload_MetricSeries{Metric: "parity.requests", Tags: []string{"route:/", "host:x"}}}}},
			{Route: "/api/v0.2/traces", ArrivedAt: base.Add(3 * time.Second), Decoded: []*aggregator.TracePayload{{AgentPayload: &pb.AgentPayload{TracerPayloads: []*pb.TracerPayload{{
				Chunks: []*pb.TraceChunk{{Spans: []*pb.Span{{Resource: "SELECT 1", SpanID: 2, ParentID: 1}, {Resource: "GET /", SpanID: 1}, {Resource: "SELECT 1", SpanID: 3, ParentID: 1}}}},
			}}}}}},
			{Route: "/api/beta/sketches", ArrivedAt: base.Add(3 * time.Second), Decoded: []*aggregator.Sketch{{SketchPayload_Sketch: metricspb.SketchPayload_Sketch{Metric: "parity.latency", Tags: []string{"host:x"}}}}},
			{Route: "/api/v1/check_run", ArrivedAt: base, Error: "no decoder"},
			{Route: "/api/v1/check_run", ArrivedAt: base.Add(6 * time.Second), Decoded: []*aggregator.CheckRun{{Check: "datadog.agent.up"}}},
		},
		Outcome: runner.Outcome{
			ExitedAt:    base.Add(9 * time.Second),
			SigtermAt:   base.Add(6 * time.Second),
			ClockOffset: 2 * time.Second,
		},
		Events: []events.Event{
			{Time: base, Kind: "started", Detail: map[string]any{}},
			{Time: base.Add(time.Second), Kind: "sent", Detail: map[string]any{"step": "send_dogstatsd", "lines": []any{"parity.requests:1|c"}}},
			{Time: base.Add(2 * time.Second), Kind: "printed", Detail: map[string]any{"stream": "stdout", "line": "parity-child: started"}},
		},
	}
	got, err := Apply(c, nil)
	if err != nil {
		t.Fatal(err)
	}
	want := timing.Input{
		Arrivals: []timing.Arrival{
			{Area: "series", At: base.Add(3 * time.Second), Items: []string{"metric:parity.requests|host:x,route:/"}},
			{Area: "sketches", At: base.Add(3 * time.Second), Items: []string{"metric:parity.latency|host:x"}},
			{Area: "traces", At: base.Add(3 * time.Second), Items: []string{"span:GET /", "span:SELECT 1"}},
			{Area: "logs", At: base.Add(4 * time.Second), Items: []string{"log:parity-child: started"}},
			{Area: "stats", At: base.Add(5 * time.Second), Items: []string{"span:GET /"}},
			{Area: "check_runs", At: base.Add(6 * time.Second), Items: []string{"check:datadog.agent.up"}},
		},
		Sent: []timing.Sent{
			{At: base.Add(time.Second), Items: []string{"metric:parity.requests"}},
			{At: base.Add(2 * time.Second), Items: []string{"log:parity-child: started"}},
		},
		Sigterm: base.Add(8 * time.Second),
		Exited:  base.Add(9 * time.Second),
	}
	if !reflect.DeepEqual(got.Timing, want) {
		t.Fatalf("got %+v\nwant %+v", got.Timing, want)
	}
}

// encodedSketch returns the protobuf DDSketch of values, as the Go agent encodes stats summaries.
func encodedSketch(t *testing.T, values ...float64) []byte {
	t.Helper()
	sketch, err := ddsketch.NewDefaultDDSketch(0.01)
	if err != nil {
		t.Fatal(err)
	}
	for _, v := range values {
		if err := sketch.Add(v); err != nil {
			t.Fatal(err)
		}
	}
	data, err := proto.Marshal(sketch.ToProto())
	if err != nil {
		t.Fatal(err)
	}
	return data
}

// knownSummary is the decoded sketch of 0, 100, 200, ... 900 at a relative accuracy of 0.01:
// gamma is 1.01/0.99, and value v falls in bin floor(log(v)/log(gamma)).
var knownSummary = Summary{
	Gamma:     1.02020202020202,
	Positive:  map[int32]float64{230: 1, 264: 1, 285: 1, 299: 1, 310: 1, 319: 1, 327: 1, 334: 1, 340: 1},
	ZeroCount: 1,
}

// fullCapture has a value for every field that a starting rule names.
func fullCapture(t *testing.T) capture.Capture {
	versions := []string{"datadog_init_version:1.2.3", "datadog_sidecar_version:1.2.3", "_dd.datadog_init_version:1.2.3", "_dd.datadog_sidecar_version:1.2.3"}
	window := runner.Outcome{StartedAt: time.Unix(1700000003, 0), ExitedAt: time.Unix(1700000008, 0)}
	return capture.Capture{Outcome: window, Payloads: []capture.Payload{
		{Route: "/api/v2/series", Decoded: []*aggregator.MetricSeries{{MetricPayload_MetricSeries: metricspb.MetricPayload_MetricSeries{
			Metric: "parity.requests", Type: metricspb.MetricPayload_COUNT,
			Tags:      append([]string{"service:parity-app", "instance:abc123def456"}, versions...),
			Resources: []*metricspb.MetricPayload_Resource{{Type: "host", Name: "abc123"}},
			Points:    []*metricspb.MetricPayload_MetricPoint{{Value: 2, Timestamp: 1700000000}},
			Metadata:  &metricspb.Metadata{Origin: &metricspb.Origin{OriginProduct: 10, OriginCategory: 11, OriginService: 12}},
		}}, {MetricPayload_MetricSeries: metricspb.MetricPayload_MetricSeries{
			// datadog.agent.running carries the flush time, which is not aligned.
			Metric: "datadog.agent.running", Type: metricspb.MetricPayload_GAUGE, Tags: []string{"version:7.85.2"},
			Resources: []*metricspb.MetricPayload_Resource{{Type: "host", Name: "abc123"}},
			Points:    []*metricspb.MetricPayload_MetricPoint{{Value: 1, Timestamp: 1700000004}},
		}}}},
		{Route: "/api/beta/sketches", Decoded: []*aggregator.Sketch{{SketchPayload_Sketch: metricspb.SketchPayload_Sketch{
			Metric: "parity.latency", Host: "abc123", Tags: append([]string{"a:b"}, versions...),
			Dogsketches:   []metricspb.SketchPayload_Sketch_Dogsketch{{Ts: 1700000000, Cnt: 1, Min: 12, Max: 12, Avg: 12, Sum: 12, K: []int32{1338}, N: []uint32{1}}},
			Distributions: []metricspb.SketchPayload_Sketch_Distribution{{Ts: 1700000000, Cnt: 1, Min: 12, Max: 12, Avg: 12, Sum: 12, V: []float64{12}, G: []uint32{1}, Delta: []uint32{0}}},
		}}}},
		{Route: "/api/v2/logs", Decoded: []*aggregator.Log{{
			Message: "parity-child: started", Status: "info", Timestamp: 1700000004000, HostName: "abc123",
			Service: "parity-app", Source: "cloudrun", Tags: append([]string{"z:1", "instance:abc123def456"}, versions...),
		}}},
		{Route: "/api/v0.2/traces", Decoded: []*aggregator.TracePayload{{AgentPayload: &pb.AgentPayload{
			HostName: "abc123", AgentVersion: "7.70.0",
			TracerPayloads: []*pb.TracerPayload{{
				ContainerID: "c0ffee", Hostname: "abc123",
				Chunks: []*pb.TraceChunk{{Priority: 1, Spans: []*pb.Span{
					{Service: "parity-app", Resource: "SELECT 1", TraceID: 1001, SpanID: 2002, ParentID: 2001, Start: 1700000000000000001, Duration: 2,
						SpanLinks:  []*pb.SpanLink{{TraceID: 1001, TraceIDHigh: 77, SpanID: 2001}},
						SpanEvents: []*pb.SpanEvent{{TimeUnixNano: 1700000000000000002, Name: "retry", Attributes: map[string]*pb.AttributeAnyValue{"attempt": {Type: pb.AttributeAnyValue_INT_VALUE, IntValue: 2}}}}},
					{Service: "parity-app", Resource: "GET /", TraceID: 1001, SpanID: 2001, Start: 1700000000000000000, Duration: 5,
						Meta: map[string]string{"_dd.datadog_init_version": "1.2.3", "_dd.datadog_sidecar_version": "1.2.3", "_dd.p.tid": "6560a4e300000000", "instance": "abc123def456",
							"_dd.install.id": "da37b810-c141-4968-8c03-e29a8e55e00a", "_dd.install.time": "1795401594", "_dd.install.type": "manual"}},
				}}},
			}},
		}}}},
		{Route: "/api/v0.2/stats", Decoded: []*aggregator.APMStatsPayload{{StatsPayload: &pb.StatsPayload{
			AgentHostname: "abc123", AgentVersion: "7.70.0",
			Stats: []*pb.ClientStatsPayload{{
				Hostname: "abc123", ContainerID: "c0ffee", Tags: []string{"b:1", "a:1"},
				Stats: []*pb.ClientStatsBucket{{Start: 1700000000000000000, Duration: 10000000000, Stats: []*pb.ClientGroupedStats{{
					Service: "parity-app", Resource: "GET /", HTTPMethod: "GET", Hits: 1, Duration: 5,
					PeerTags: []string{"y:1", "x:1"}, SpanDerivedPrimaryTags: []string{"q:1", "p:1"}, AdditionalMetricTags: []string{"n:1", "m:1"},
					OkSummary: encodedSketch(t, 0, 100, 200, 300, 400, 500, 600, 700, 800, 900), ErrorSummary: encodedSketch(t),
				}}}},
			}},
		}}}},
		{Route: "/api/v1/check_run", Decoded: []*aggregator.CheckRun{{
			Check: "datadog.agent.up", HostName: "abc123", Timestamp: 1700000005, Status: 0, Tags: []string{"b:1", "a:1"},
		}}},
	}}
}

// TestStartingRules checks that every rule in normalize.yaml reaches the field it names.
func TestStartingRules(t *testing.T) {
	rules, tolerances, err := LoadRules("../../normalize.yaml")
	if err != nil {
		t.Fatal(err)
	}
	want := timing.Tolerances{Interval: time.Second, Delay: time.Second, Shutdown: time.Second, Percentile: 0.02, FlushGap: 500 * time.Millisecond}
	if !reflect.DeepEqual(tolerances, want) {
		t.Fatalf("tolerances %+v, want %+v", tolerances, want)
	}
	got, err := Apply(fullCapture(t), rules)
	if err != nil {
		t.Fatal(err)
	}
	for _, rule := range got.UnmatchedRules {
		t.Errorf("rule %s %s matched nothing", rule.Action, rule.Path)
	}
	versions := []string{"_dd.datadog_init_version:<placeholder>", "_dd.datadog_sidecar_version:<placeholder>", "datadog_init_version:<placeholder>", "datadog_sidecar_version:<placeholder>"}
	wantSeries := []Series{{Metric: "parity.requests", Type: "count", Host: "<masked>", Origin: &Origin{Product: 10, Category: 11, Service: 12},
		Tags: append(slices.Clone(versions), "instance:<masked>", "service:parity-app"), Points: []Point{{Timestamp: Timestamp{Label: "in window, aligned"}, Value: 2}}},
		{Metric: "datadog.agent.running", Type: "gauge", Host: "<masked>", Tags: []string{"version:<placeholder>"}, Points: []Point{{Timestamp: Timestamp{Label: "in window"}, Value: 1}}}}
	wantSketches := []Sketch{{Metric: "parity.latency", Host: "<masked>", Tags: []string{versions[0], versions[1], "a:b", versions[2], versions[3]},
		Points:        []SketchPoint{{Timestamp: Timestamp{Label: "in window, aligned"}, Count: 1, Min: 12, Max: 12, Avg: 12, Sum: 12, K: []int32{1338}, N: []uint32{1}}},
		Distributions: []Distribution{{Timestamp: Timestamp{Label: "in window, aligned"}, Count: 1, Min: 12, Max: 12, Avg: 12, Sum: 12, V: []float64{12}, G: []uint32{1}, Delta: []uint32{0}}}}}
	wantLogs := []Log{{Message: "parity-child: started", Status: "info", Timestamp: Timestamp{Label: "in window"}, Hostname: "<masked>", Service: "parity-app", Source: "cloudrun",
		Tags: append(slices.Clone(versions), "instance:<masked>", "z:1")}}
	wantTraces := []TracePayload{{HostName: "<masked>", AgentVersion: "<placeholder>", TracerPayloads: []TracerPayload{{ContainerID: "<masked>", Hostname: "<masked>",
		Chunks: []Chunk{{Priority: 1, Spans: []Span{
			{Service: "parity-app", Resource: "GET /", TraceID: "trace-1", SpanID: "span-1", Start: 1700000000000000000, Duration: 5,
				Meta: map[string]string{"_dd.datadog_init_version": "<placeholder>", "_dd.datadog_sidecar_version": "<placeholder>", "_dd.p.tid": "<masked>", "instance": "<masked>",
					"_dd.install.id": "<masked>", "_dd.install.time": "<masked>", "_dd.install.type": "manual"}},
			{Service: "parity-app", Resource: "SELECT 1", TraceID: "trace-1", SpanID: "span-2", ParentID: "span-1", Start: 1700000000000000001, Duration: 2,
				Links:  []SpanLink{{TraceID: "trace-1", TraceIDHigh: "<masked>", SpanID: "span-1"}},
				Events: []SpanEvent{{TimeUnixNano: 1, Name: "retry", Attributes: map[string]AttributeValue{"attempt": {Type: "int", Int: 2}}}}},
		}}}}}}}
	wantStats := []StatsPayload{{AgentHostname: "<masked>", AgentVersion: "<placeholder>", Stats: []ClientStats{{Hostname: "<masked>", ContainerID: "<masked>", Tags: []string{"a:1", "b:1"},
		Buckets: []StatsBucket{{Start: Timestamp{Label: "in window, aligned"}, Duration: 10000000000, Stats: []GroupedStats{{
			Service: "parity-app", Resource: "GET /", HTTPMethod: "GET", Hits: 1, Duration: 5,
			PeerTags: []string{"x:1", "y:1"}, SpanDerivedPrimaryTags: []string{"p:1", "q:1"}, AdditionalMetricTags: []string{"m:1", "n:1"},
			OkSummary: &knownSummary, ErrorSummary: &Summary{Gamma: 1.02020202020202},
		}}}}}}}}
	for _, check := range []struct {
		name      string
		got, want any
	}{
		{"series", got.Series, wantSeries},
		{"sketches", got.Sketches, wantSketches},
		{"logs", got.Logs, wantLogs},
		{"traces", got.Traces, wantTraces},
		{"stats", got.Stats, wantStats},
		{"check runs", got.CheckRuns, []CheckRun{{Check: "datadog.agent.up", HostName: "<masked>", Timestamp: Timestamp{Label: "in window"}, Tags: []string{"a:1", "b:1"}}}},
	} {
		if !reflect.DeepEqual(check.got, check.want) {
			t.Errorf("%s: got %+v\nwant %+v", check.name, check.got, check.want)
		}
	}
}

// DogStatsD and enhanced series carry the start of a 10-second bucket, so normalize.yaml checks
// their alignment; datadog.agent.running carries the flush time and is only checked for its window.
func TestSeriesTimestampAlignment(t *testing.T) {
	rules, _, err := LoadRules("../../normalize.yaml")
	if err != nil {
		t.Fatal(err)
	}
	point := func(metric string, timestamp int64) *aggregator.MetricSeries {
		return &aggregator.MetricSeries{MetricPayload_MetricSeries: metricspb.MetricPayload_MetricSeries{Metric: metric, Type: metricspb.MetricPayload_GAUGE,
			Points: []*metricspb.MetricPayload_MetricPoint{{Value: 1, Timestamp: timestamp}}}}
	}
	c := capture.Capture{Outcome: runner.Outcome{StartedAt: time.Unix(1700000003, 0), ExitedAt: time.Unix(1700000008, 0)},
		Payloads: []capture.Payload{{Route: "/api/v2/series", Decoded: []*aggregator.MetricSeries{
			point("parity.gauge", 1700000004), point("parity.gauge", 1700000000), point("datadog.agent.running", 1700000004),
		}}}}
	got, err := Apply(c, rules)
	if err != nil {
		t.Fatal(err)
	}
	var labels []string
	for _, series := range got.Series {
		labels = append(labels, series.Metric+" "+series.Points[0].Timestamp.Label)
	}
	want := []string{"parity.gauge not aligned: 1700000004", "parity.gauge in window, aligned", "datadog.agent.running in window"}
	if !slices.Equal(labels, want) {
		t.Fatalf("got %q, want %q", labels, want)
	}
}

func TestMaskKeepsPresence(t *testing.T) {
	for _, tc := range []struct {
		name      string
		host      string
		timestamp int64
		want      Series
	}{
		{"present", "abc123", 1700000000, Series{Metric: "m", Type: "count", Host: "<masked>", Points: []Point{{Timestamp: Timestamp{Value: 1}, Value: 1}}}},
		{"absent", "", 0, Series{Metric: "m", Type: "count", Points: []Point{{Value: 1}}}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			series := metricspb.MetricPayload_MetricSeries{Metric: "m", Type: metricspb.MetricPayload_COUNT,
				Points: []*metricspb.MetricPayload_MetricPoint{{Value: 1, Timestamp: tc.timestamp}}}
			if tc.host != "" {
				series.Resources = []*metricspb.MetricPayload_Resource{{Type: "host", Name: tc.host}}
			}
			c := capture.Capture{Payloads: []capture.Payload{{Route: "/api/v2/series", Decoded: []*aggregator.MetricSeries{{MetricPayload_MetricSeries: series}}}}}
			got, err := Apply(c, []Rule{
				{Action: "mask", Path: "series[].host", Reason: "test"},
				{Action: "mask", Path: "series[].points[].timestamp", Reason: "test"},
			})
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(got.Series, []Series{tc.want}) {
				t.Fatalf("got %+v, want %+v", got.Series, tc.want)
			}
		})
	}
}

func TestMaskWhenMatches(t *testing.T) {
	rule := Rule{Action: "mask", Path: "series[].tags[instance]", WhenMatches: "[0-9a-f]{12}", Reason: "test"}
	for _, tc := range []struct{ name, tag, want string }{
		{"Docker container hostname", "instance:3f2a9c1b7d4e", "instance:<masked>"},
		{"Cloud Run instance ID", "instance:0000000000000000parityinstance01", "instance:0000000000000000parityinstance01"},
		{"hostname inside a longer value", "instance:x3f2a9c1b7d4e", "instance:x3f2a9c1b7d4e"},
		{"unknown", "instance:unknown", "instance:unknown"},
		{"empty", "instance:", "instance:"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got, err := Apply(seriesCapture(tc.tag), []Rule{rule})
			if err != nil {
				t.Fatal(err)
			}
			if len(got.Series) != 1 || !reflect.DeepEqual(got.Series[0].Tags, []string{tc.want}) {
				t.Fatalf("got %+v, want tags [%s]", got.Series, tc.want)
			}
		})
	}
}

// TestPermutedTracesNormalizeTheSame checks that relabeling does not depend on payload, chunk or
// span order, or on the raw IDs.
func TestPermutedTracesNormalizeTheSame(t *testing.T) {
	rules, _, err := LoadRules("../../normalize.yaml")
	if err != nil {
		t.Fatal(err)
	}
	span := func(resource string, traceID, spanID, parentID uint64) *pb.Span {
		return &pb.Span{Service: "parity-app", Resource: resource, TraceID: traceID, SpanID: spanID, ParentID: parentID}
	}
	payload := func(chunks ...*pb.TraceChunk) []*aggregator.TracePayload {
		return []*aggregator.TracePayload{{AgentPayload: &pb.AgentPayload{TracerPayloads: []*pb.TracerPayload{{Chunks: chunks}}}}}
	}
	base := time.Date(2026, 11, 22, 10, 0, 0, 0, time.UTC)
	first := capture.Capture{Payloads: []capture.Payload{
		{Route: "/api/v0.2/traces", ArrivedAt: base, Decoded: payload(
			&pb.TraceChunk{Spans: []*pb.Span{span("GET /", 1, 10, 0), span("SELECT 1", 1, 11, 10), span("GET key", 1, 12, 10)}},
			&pb.TraceChunk{Spans: []*pb.Span{span("GET /fail", 2, 20, 0)}},
		)},
		{Route: "/api/v0.2/traces", ArrivedAt: base.Add(time.Second), Decoded: payload(
			&pb.TraceChunk{Spans: []*pb.Span{span("POST /", 3, 30, 0)}},
		)},
	}}
	second := capture.Capture{Payloads: []capture.Payload{
		{Route: "/api/v0.2/traces", ArrivedAt: base.Add(time.Second), Decoded: payload(
			&pb.TraceChunk{Spans: []*pb.Span{span("POST /", 9, 93, 0)}},
		)},
		{Route: "/api/v0.2/traces", ArrivedAt: base, Decoded: payload(
			&pb.TraceChunk{Spans: []*pb.Span{span("GET /fail", 8, 83, 0)}},
			&pb.TraceChunk{Spans: []*pb.Span{span("GET key", 7, 71, 77), span("SELECT 1", 7, 72, 77), span("GET /", 7, 77, 0)}},
		)},
	}}
	a, err := Apply(first, rules)
	if err != nil {
		t.Fatal(err)
	}
	b, err := Apply(second, rules)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(a, b) {
		t.Fatalf("permuted captures differ:\n%+v\n%+v", a.Traces, b.Traces)
	}
	wantSpans := []Span{
		{Service: "parity-app", Resource: "GET /", TraceID: "trace-1", SpanID: "span-1"},
		{Service: "parity-app", Resource: "GET key", TraceID: "trace-1", SpanID: "span-2", ParentID: "span-1"},
		{Service: "parity-app", Resource: "SELECT 1", TraceID: "trace-1", SpanID: "span-3", ParentID: "span-1"},
	}
	if spans := a.Traces[0].TracerPayloads[0].Chunks[0].Spans; !reflect.DeepEqual(spans, wantSpans) {
		t.Fatalf("got %+v, want %+v", spans, wantSpans)
	}
}

func TestMaskRejectsLists(t *testing.T) {
	_, err := Apply(seriesCapture("a:1"), []Rule{{Action: "mask", Path: "series[].tags", Reason: "test"}})
	if err == nil {
		t.Fatal("got no error")
	}
}

func TestUnmatchedRules(t *testing.T) {
	rules := []Rule{
		{Action: "sort", Path: "series[].tags", Reason: "test"},
		{Action: "mask", Path: "sketches[].host", Reason: "test"},
		{Action: "mask", Path: "series[].hots", Reason: "test"},
	}
	got, err := Apply(seriesCapture("a:1"), rules)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got.UnmatchedRules, rules[1:]) {
		t.Fatalf("got %+v, want %+v", got.UnmatchedRules, rules[1:])
	}
}

func TestUndecodedPayloadsAreListed(t *testing.T) {
	for _, tc := range []struct {
		name    string
		payload capture.Payload
		want    string
	}{
		{"route without a decoder", capture.Payload{Route: "/api/v1/unknown", Error: "no decoder"}, "no decoder"},
		{"summary that does not decode", capture.Payload{Route: "/api/v0.2/stats", Decoded: []*aggregator.APMStatsPayload{{StatsPayload: &pb.StatsPayload{
			Stats: []*pb.ClientStatsPayload{{Stats: []*pb.ClientStatsBucket{{Stats: []*pb.ClientGroupedStats{{Service: "s", Name: "n", Resource: "r", Hits: 1, OkSummary: []byte{0xff}}}}}}},
		}}}}, "decoding the ok summary of s n r: "},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got, err := Apply(capture.Capture{Payloads: []capture.Payload{tc.payload}}, nil)
			if err != nil {
				t.Fatal(err)
			}
			if len(got.Undecoded) != 1 || got.Undecoded[0].Route != tc.payload.Route || !strings.HasPrefix(got.Undecoded[0].Error, tc.want) {
				t.Fatalf("got %+v, want one entry for %s starting %q", got.Undecoded, tc.payload.Route, tc.want)
			}
		})
	}
}

func TestDecodeSummary(t *testing.T) {
	for _, tc := range []struct {
		name    string
		raw     []byte
		want    *Summary
		wantErr bool
	}{
		{"known sketch", encodedSketch(t, 0, 100, 200, 300, 400, 500, 600, 700, 800, 900), &knownSummary, false},
		{"empty sketch", encodedSketch(t), &Summary{Gamma: 1.02020202020202}, false},
		{"no bytes", nil, nil, false},
		{"not a sketch", []byte{0xff}, nil, true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got, err := decodeSummary(tc.raw)
			if (err != nil) != tc.wantErr {
				t.Fatalf("got error %v, want an error: %v", err, tc.wantErr)
			}
			if !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("got %+v, want %+v", got, tc.want)
			}
		})
	}
}

// TestCanonicalOrderIgnoresRunSpecificValues checks that values the rules mask, and sibling
// order, do not change the labels.
func TestCanonicalOrderIgnoresRunSpecificValues(t *testing.T) {
	rules, _, err := LoadRules("../../normalize.yaml")
	if err != nil {
		t.Fatal(err)
	}
	span := func(resource string, spanID, parentID uint64, tid string) *pb.Span {
		s := &pb.Span{Service: "parity-app", Resource: resource, TraceID: spanID / 10, SpanID: spanID, ParentID: parentID}
		if tid != "" {
			s.Meta = map[string]string{"_dd.p.tid": tid}
		}
		return s
	}
	traces := func(chunks ...*pb.TraceChunk) capture.Capture {
		return capture.Capture{Payloads: []capture.Payload{{Route: "/api/v0.2/traces", Decoded: []*aggregator.TracePayload{{AgentPayload: &pb.AgentPayload{
			TracerPayloads: []*pb.TracerPayload{{Chunks: chunks}},
		}}}}}}
	}
	for _, tc := range []struct {
		name          string
		first, second capture.Capture
	}{
		{"trace ID high bits swapped",
			traces(
				&pb.TraceChunk{Spans: []*pb.Span{span("GET /", 10, 0, "aaaa"), span("SELECT 1", 11, 10, "")}},
				&pb.TraceChunk{Spans: []*pb.Span{span("GET /", 20, 0, "bbbb"), span("SELECT 2", 21, 20, "")}},
			),
			traces(
				&pb.TraceChunk{Spans: []*pb.Span{span("GET /", 10, 0, "bbbb"), span("SELECT 1", 11, 10, "")}},
				&pb.TraceChunk{Spans: []*pb.Span{span("GET /", 20, 0, "aaaa"), span("SELECT 2", 21, 20, "")}},
			)},
		{"equal siblings with different subtrees",
			traces(&pb.TraceChunk{Spans: []*pb.Span{
				span("GET /", 10, 0, ""), span("call", 11, 10, ""), span("call", 12, 10, ""), span("a", 13, 11, ""), span("b", 14, 12, ""),
			}}),
			traces(&pb.TraceChunk{Spans: []*pb.Span{
				span("GET /", 10, 0, ""), span("call", 12, 10, ""), span("call", 11, 10, ""), span("b", 14, 12, ""), span("a", 13, 11, ""),
			}})},
	} {
		t.Run(tc.name, func(t *testing.T) {
			a, err := Apply(tc.first, rules)
			if err != nil {
				t.Fatal(err)
			}
			b, err := Apply(tc.second, rules)
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(a.Traces, b.Traces) {
				t.Fatalf("captures differ:\n%+v\n%+v", a.Traces, b.Traces)
			}
		})
	}
}

// TestGaugeOrderIsKept checks that series keep arrival order, so a different last gauge value
// still differs.
func TestGaugeOrderIsKept(t *testing.T) {
	rules, _, err := LoadRules("../../normalize.yaml")
	if err != nil {
		t.Fatal(err)
	}
	gauges := func(values ...float64) capture.Capture {
		var c capture.Capture
		for i, v := range values {
			c.Payloads = append(c.Payloads, capture.Payload{Route: "/api/v2/series", Decoded: []*aggregator.MetricSeries{{MetricPayload_MetricSeries: metricspb.MetricPayload_MetricSeries{
				Metric: "parity.gauge", Type: metricspb.MetricPayload_GAUGE,
				Points: []*metricspb.MetricPayload_MetricPoint{{Value: v, Timestamp: 1700000000 + int64(i)}},
			}}}})
		}
		return c
	}
	a, err := Apply(gauges(1, 2), rules)
	if err != nil {
		t.Fatal(err)
	}
	b, err := Apply(gauges(2, 1), rules)
	if err != nil {
		t.Fatal(err)
	}
	if reflect.DeepEqual(a.Series, b.Series) {
		t.Fatalf("gauges ending on different values normalize the same: %+v", a.Series)
	}
	if last := a.Series[len(a.Series)-1].Points[0].Value; last != 2 {
		t.Fatalf("last gauge value %v, want 2", last)
	}
}

// The stop time is the harness time converted with the estimated clock offset, or the driver's
// signal event on the Docker clock when that is earlier.
func TestTimingSigterm(t *testing.T) {
	base := time.Date(2026, 11, 22, 10, 0, 0, 0, time.UTC)
	outcome := runner.Outcome{SigtermAt: base, StopSignal: "SIGTERM", ClockOffset: 30 * time.Millisecond, ExitedAt: base.Add(time.Second)}
	signal := func(name string, after time.Duration) []events.Event {
		return []events.Event{{Time: base.Add(after), Kind: "signal", Detail: map[string]any{"signal": name}}}
	}
	for _, tc := range []struct {
		name   string
		events []events.Event
		want   time.Time
	}{
		{"no signal event", nil, base.Add(30 * time.Millisecond)},
		{"earlier signal event", signal("SIGTERM", 20*time.Millisecond), base.Add(20 * time.Millisecond)},
		{"later signal event", signal("SIGTERM", 2*time.Second), base.Add(30 * time.Millisecond)},
		{"another signal", signal("SIGINT", 20*time.Millisecond), base.Add(30 * time.Millisecond)},
		{"too far before the estimate", signal("SIGTERM", -100*time.Millisecond), base.Add(30 * time.Millisecond)},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got, err := timingInput(capture.Capture{Outcome: outcome, Events: tc.events})
			if err != nil {
				t.Fatal(err)
			}
			if !got.Sigterm.Equal(tc.want) {
				t.Fatalf("sigterm %v, want %v", got.Sigterm, tc.want)
			}
		})
	}
}

// The relative action checks a timestamp against the run window, from the container start to its
// exit with a one-second margin, and against its declared unit and alignment.
func TestRelativeLabel(t *testing.T) {
	start := time.Unix(1700000003, 500_000_000)
	exit := time.Unix(1700000020, 0)
	seconds := Relative{Unit: "s"}
	buckets := Relative{Unit: "s", Align: 10 * time.Second, Lead: 10 * time.Second}
	for _, tc := range []struct {
		name     string
		relative Relative
		value    int64
		want     string
	}{
		{"seconds in the window", seconds, 1700000010, "in window"},
		{"seconds truncated just before the start", seconds, 1700000003, "in window"},
		{"seconds within the margin after the exit", seconds, 1700000021, "in window"},
		{"seconds after the margin", seconds, 1700000022, "out of window: 1700000022"},
		{"seconds before the margin", seconds, 1700000001, "out of window: 1700000001"},
		{"seconds declared as milliseconds", Relative{Unit: "ms"}, 1700000010, "out of window: 1700000010"},
		{"milliseconds in the window", Relative{Unit: "ms"}, 1700000010123, "in window"},
		{"microseconds declared as nanoseconds", Relative{Unit: "ns"}, 1700000010000000, "out of window: 1700000010000000"},
		{"nanoseconds declared as seconds", seconds, 1700000010000000000, "out of window: 1700000010000000000"},
		{"an aligned bucket that starts before the container", buckets, 1700000000, "in window, aligned"},
		{"an aligned bucket in the window", buckets, 1700000010, "in window, aligned"},
		{"an unaligned bucket", buckets, 1700000013, "not aligned: 1700000013"},
		{"a bucket before the lead", buckets, 1699999990, "out of window: 1699999990"},
		{"aligned nanoseconds", Relative{Unit: "ns", Align: 10 * time.Second, Lead: 10 * time.Second}, 1700000010000000000, "in window, aligned"},
		{"unaligned nanoseconds", Relative{Unit: "ns", Align: 10 * time.Second, Lead: 10 * time.Second}, 1700000010000001000, "not aligned: 1700000010000001000"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := relativeLabel(tc.value, tc.relative, start, exit); got != tc.want {
				t.Fatalf("got %q, want %q", got, tc.want)
			}
		})
	}
	if got := relativeLabel(1700000010, seconds, time.Time{}, time.Time{}); got != "no run window: 1700000010" {
		t.Fatalf("without a run window, got %q", got)
	}
}

// A trace or stats payload that decodes to nothing was not compared, so it is undecoded.
func TestPayloadWithoutContentIsUndecoded(t *testing.T) {
	c := capture.Capture{Payloads: []capture.Payload{
		{Route: "/api/v0.2/traces", Decoded: []*aggregator.TracePayload{{}}},
		{Route: "/api/v0.2/stats", Decoded: []*aggregator.APMStatsPayload{{}}},
	}}
	got, err := Apply(c, nil)
	if err != nil {
		t.Fatal(err)
	}
	want := []Undecoded{
		{Route: "/api/v0.2/traces", Error: "the trace payload has no agent payload"},
		{Route: "/api/v0.2/stats", Error: "the stats payload has no content"},
	}
	if !reflect.DeepEqual(got.Undecoded, want) || len(got.Traces) != 0 || len(got.Stats) != 0 {
		t.Fatalf("got undecoded %+v, traces %+v, stats %+v; want undecoded %+v and no traces or stats", got.Undecoded, got.Traces, got.Stats, want)
	}
}

// An agent that outlived its deadline is a process difference, not a harness error.
func TestProcessViewKeepsNotExited(t *testing.T) {
	message := "the agent did not exit within 20 seconds after the workload finished"
	got, err := Apply(capture.Capture{Outcome: runner.Outcome{ExitCode: 143, NotExited: message}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if want := (ProcessView{ExitCode: 143, NotExited: message}); !reflect.DeepEqual(got.Process, want) {
		t.Fatalf("got %+v, want %+v", got.Process, want)
	}
}
