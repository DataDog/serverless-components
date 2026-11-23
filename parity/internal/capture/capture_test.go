package capture

import (
	"bytes"
	"encoding/json"
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"testing"
	"time"

	metricspb "github.com/DataDog/agent-payload/v5/gogen"
	"github.com/DataDog/datadog-agent/test/fakeintake/aggregator"
	"github.com/DataDog/datadog-agent/test/fakeintake/api"
	"github.com/DataDog/datadog-agent/test/fakeintake/server"
	"github.com/benbjohnson/clock"

	"github.com/DataDog/serverless-components/parity/internal/events"
	"github.com/DataDog/serverless-components/parity/internal/runner"
)

func post(t *testing.T, url, route, contentType string, body []byte) {
	t.Helper()
	resp, err := http.Post(url+route, contentType, bytes.NewReader(body))
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode >= 300 {
		t.Fatalf("posting to %s returned status %d", route, resp.StatusCode)
	}
}

func TestCaptureDecodesSeriesAndKeepsArrivalTime(t *testing.T) {
	fi, mockClock := newIntake(t)
	// The mock clock starts at the Unix epoch. Setting it to a current date makes the fakeintake
	// mock replay millions of cleanup ticks and hang, so advance it by a few seconds.
	mockClock.Add(5 * time.Second)
	series := `{"series":[{"metric":"parity.test","type":1,"points":[{"timestamp":1700000000,"value":2}]}]}`
	post(t, fi.URL(), "/api/v2/series", "application/json", []byte(series))

	got, err := Collect(fi.URL(), runner.Outcome{})
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Payloads) != 1 || got.Routes["/api/v2/series"] != 1 {
		t.Fatalf("expected one series payload, got %+v with routes %v", got.Payloads, got.Routes)
	}
	p := got.Payloads[0]
	if p.Error != "" {
		t.Fatalf("unexpected decode error %q", p.Error)
	}
	if want := time.Unix(5, 0); !p.ArrivedAt.Equal(want) {
		t.Fatalf("arrival time %v, want %v", p.ArrivedAt, want)
	}
	encoded, err := json.Marshal(p.Decoded)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Contains(encoded, []byte("parity.test")) {
		t.Fatalf("decoded payload %s does not contain the metric name", encoded)
	}
}

func TestDecodeCheckRuns(t *testing.T) {
	body := `[{"check":"datadog.agent.up","host_name":"abc123","timestamp":1700000000,"status":0,"message":"","tags":["a:1"]}]`
	got := decode("/api/v1/check_run", api.Payload{Data: []byte(body)})
	if got.Error != "" {
		t.Fatalf("unexpected decode error %q", got.Error)
	}
	runs, ok := got.Decoded.([]*aggregator.CheckRun)
	if !ok || len(runs) != 1 || runs[0].Check != "datadog.agent.up" || runs[0].HostName != "abc123" {
		t.Fatalf("decoded %#v, want one datadog.agent.up check run", got.Decoded)
	}
}

func TestCaptureKeepsUnknownRoutesRaw(t *testing.T) {
	fi, _ := newIntake(t)
	post(t, fi.URL(), "/api/v1/unknown", "text/plain", []byte("hello"))

	got, err := Collect(fi.URL(), runner.Outcome{})
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Payloads) != 1 {
		t.Fatalf("expected one payload, got %+v", got.Payloads)
	}
	p := got.Payloads[0]
	if p.Route != "/api/v1/unknown" || p.Error != "no decoder" || p.Decoded == nil {
		t.Fatalf("unexpected payload %+v", p)
	}
}

func TestCaptureRecordsUndecodablePayload(t *testing.T) {
	fi, _ := newIntake(t)
	post(t, fi.URL(), "/api/v2/series", "application/x-protobuf", []byte{0xff, 0xff, 0xff, 0xff, 0x01})

	got, err := Collect(fi.URL(), runner.Outcome{})
	if err != nil {
		t.Fatalf("an undecodable payload must not fail the capture: %v", err)
	}
	if len(got.Payloads) != 1 || got.Payloads[0].Error == "" {
		t.Fatalf("expected one payload with an error, got %+v", got.Payloads)
	}
}

func TestCollectReadsEventLogAndWriteJSON(t *testing.T) {
	fi, _ := newIntake(t)
	logPath := filepath.Join(t.TempDir(), "events.jsonl")
	event := events.Event{Time: time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC), Kind: "started"}
	if err := events.Append(logPath, event); err != nil {
		t.Fatal(err)
	}
	got, err := Collect(fi.URL(), runner.Outcome{EventLogPath: logPath})
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Events) != 1 || got.Events[0].Kind != "started" {
		t.Fatalf("unexpected events %+v", got.Events)
	}
	got.Scenario, got.Binary = "basic", "go"
	out := filepath.Join(t.TempDir(), "capture.raw.json")
	if err := got.WriteJSON(out); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(out)
	if err != nil {
		t.Fatal(err)
	}
	var back Capture
	if err := json.Unmarshal(data, &back); err != nil {
		t.Fatal(err)
	}
	if back.Scenario != "basic" || len(back.Events) != 1 {
		t.Fatalf("round trip lost data: %s", data)
	}
}

func newIntake(t *testing.T) (*server.Server, *clock.Mock) {
	t.Helper()
	fi, mockClock := server.InitialiseForTests(t)
	t.Cleanup(func() { _ = fi.Stop() })
	return fi, mockClock
}

type fakeIntake struct {
	routes map[string]int
	failOn string
}

func (f fakeIntake) RouteStats() (map[string]int, error) { return f.routes, nil }

func (f fakeIntake) GetRawPayloads(route string) ([]api.Payload, error) {
	if route == f.failOn {
		return nil, errors.New("connection reset")
	}
	return []api.Payload{{Data: []byte("x")}}, nil
}

func TestCollectRecordsUnreadableRouteAndContinues(t *testing.T) {
	got, err := collect(fakeIntake{routes: map[string]int{"/a": 1, "/b": 1}, failOn: "/a"}, runner.Outcome{})
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Payloads) != 2 {
		t.Fatalf("expected two payloads, got %+v", got.Payloads)
	}
	if p := got.Payloads[0]; p.Route != "/a" || p.Error != "reading the payloads: connection reset" {
		t.Fatalf("unexpected payload for the unreadable route: %+v", p)
	}
	if p := got.Payloads[1]; p.Route != "/b" || p.Error != "no decoder" {
		t.Fatalf("unexpected payload for the readable route: %+v", p)
	}
}

func TestCollectSkipsMissingEventLog(t *testing.T) {
	got, err := collect(fakeIntake{}, runner.Outcome{EventLogPath: filepath.Join(t.TempDir(), "missing.jsonl")})
	if err != nil || len(got.Events) != 0 {
		t.Fatalf("expected no events and no error, got %+v and %v", got.Events, err)
	}
}

// A JSON series payload names the origin category and service origin_sub_product and
// origin_product_detail; they fill only what fakeintake left at zero. Protobuf payloads keep what
// fakeintake decoded.
func TestDecodeSeriesOrigin(t *testing.T) {
	protobuf, err := (&metricspb.MetricPayload{Series: []*metricspb.MetricPayload_MetricSeries{{
		Metric:   "parity.proto",
		Metadata: &metricspb.Metadata{Origin: &metricspb.Origin{OriginProduct: 1, OriginCategory: 36, OriginService: 472}},
	}}}).Marshal()
	if err != nil {
		t.Fatal(err)
	}
	cases := []struct {
		name, contentType, body string
		want                    []metricspb.Origin
	}{
		{
			name:        "json agent names",
			contentType: "application/json",
			body: `{"series":[` +
				`{"metric":"a","metadata":{"origin":{"origin_product":1,"origin_sub_product":37,"origin_product_detail":474}}},` +
				`{"metric":"b"},` +
				`{"metric":"c","metadata":{"origin":{"origin_product":1}}}]}`,
			want: []metricspb.Origin{{OriginProduct: 1, OriginCategory: 37, OriginService: 474}, {}, {OriginProduct: 1}},
		},
		{
			name:        "json protobuf names are kept",
			contentType: "application/json",
			body:        `{"series":[{"metric":"a","metadata":{"origin":{"origin_product":1,"origin_category":36,"origin_service":472,"origin_sub_product":37,"origin_product_detail":474}}}]}`,
			want:        []metricspb.Origin{{OriginProduct: 1, OriginCategory: 36, OriginService: 472}},
		},
		{
			name:        "protobuf",
			contentType: "application/x-protobuf",
			body:        string(protobuf),
			want:        []metricspb.Origin{{OriginProduct: 1, OriginCategory: 36, OriginService: 472}},
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := decode("/api/v2/series", api.Payload{Data: []byte(c.body), ContentType: c.contentType})
			if got.Error != "" {
				t.Fatalf("decode error %q", got.Error)
			}
			series := got.Decoded.([]*aggregator.MetricSeries)
			if len(series) != len(c.want) {
				t.Fatalf("got %d series, want %d", len(series), len(c.want))
			}
			for i, want := range c.want {
				var origin metricspb.Origin
				if series[i].Metadata != nil && series[i].Metadata.Origin != nil {
					origin = *series[i].Metadata.Origin
				}
				if origin.OriginProduct != want.OriginProduct || origin.OriginCategory != want.OriginCategory || origin.OriginService != want.OriginService {
					t.Errorf("series %d origin %+v, want %+v", i, origin, want)
				}
			}
		})
	}
}
