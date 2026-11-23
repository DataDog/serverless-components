// Package capture collects and decodes everything fakeintake received during one run.
package capture

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"sort"
	"time"

	metricspb "github.com/DataDog/agent-payload/v5/gogen"
	"github.com/DataDog/datadog-agent/test/fakeintake/aggregator"
	"github.com/DataDog/datadog-agent/test/fakeintake/api"
	"github.com/DataDog/datadog-agent/test/fakeintake/client"

	"github.com/DataDog/serverless-components/parity/internal/events"
	"github.com/DataDog/serverless-components/parity/internal/runner"
)

// Payload is one request that fakeintake received.
type Payload struct {
	Route string
	// ArrivedAt is the time fakeintake recorded, which is on the Docker clock.
	ArrivedAt time.Time
	// Decoded is the decoded payload, or the raw payload when Error is set.
	Decoded any
	// Error says why Decoded is not a decoded payload. It is empty on success.
	Error string
}

// Capture is everything one run produced.
type Capture struct {
	Scenario, Binary string
	Payloads         []Payload
	// Routes is the number of payloads fakeintake stored for each route.
	Routes  map[string]int
	Outcome runner.Outcome
	// Events is the workload driver's event log.
	Events []events.Event
}

// decoders maps each route the harness understands to a function that decodes its payload.
var decoders = map[string]func(api.Payload) (any, error){
	"/api/v2/series":     decoder(parseMetricSeries),
	"/api/beta/sketches": decoder(aggregator.ParseSketches),
	"/api/v2/logs":       decoder(aggregator.ParseLogPayload),
	"/api/v0.2/traces":   decoder(aggregator.ParseTracePayload),
	"/api/v0.2/stats":    decoder(aggregator.ParseAPMStatsPayload),
	"/api/v1/check_run":  decoder(aggregator.ParseCheckRunPayload),
}

func decoder[T any](parse func(api.Payload) (T, error)) func(api.Payload) (any, error) {
	return func(p api.Payload) (any, error) { return parse(p) }
}

// jsonSeriesOrigins is the origin of each series of a JSON series payload, under the names of the
// agent's JSON series format.
type jsonSeriesOrigins struct {
	Series []struct {
		Metadata *struct {
			Origin *struct {
				SubProduct    uint32 `json:"origin_sub_product"`
				ProductDetail uint32 `json:"origin_product_detail"`
			} `json:"origin"`
		} `json:"metadata"`
	} `json:"series"`
}

// parseMetricSeries decodes a series payload with fakeintake. fakeintake reads a JSON payload with
// the protobuf JSON names, origin_category and origin_service, but the agent's JSON series format
// names them origin_sub_product and origin_product_detail, so for a JSON payload those are read
// too and fill a category or service that fakeintake left at zero, matching series by index.
// Protobuf payloads are unchanged.
func parseMetricSeries(p api.Payload) ([]*aggregator.MetricSeries, error) {
	series, err := aggregator.ParseMetricSeries(p)
	if err != nil || p.ContentType != "application/json" || len(series) == 0 {
		return series, err
	}
	inflated, err := aggregator.Inflate(p.Data, p.Encoding)
	if err != nil {
		return nil, err
	}
	var origins jsonSeriesOrigins
	if err := json.Unmarshal(inflated, &origins); err != nil {
		return nil, err
	}
	for i, s := range origins.Series {
		if i >= len(series) || s.Metadata == nil || s.Metadata.Origin == nil {
			continue
		}
		if series[i].Metadata == nil {
			series[i].Metadata = &metricspb.Metadata{}
		}
		if series[i].Metadata.Origin == nil {
			series[i].Metadata.Origin = &metricspb.Origin{}
		}
		origin := series[i].Metadata.Origin
		if origin.OriginCategory == 0 {
			origin.OriginCategory = s.Metadata.Origin.SubProduct
		}
		if origin.OriginService == 0 {
			origin.OriginService = s.Metadata.Origin.ProductDetail
		}
	}
	return series, nil
}

// intake is the part of the fakeintake client that Collect uses.
type intake interface {
	RouteStats() (map[string]int, error)
	GetRawPayloads(route string) ([]api.Payload, error)
}

// Collect reads every payload fakeintake holds at fakeintakeURL and the event log named by
// outcome. A payload that cannot be decoded is kept raw with its error, and a route whose
// payloads cannot be read becomes a payload with that route and an error. A failure to read the
// route statistics, or an event log that exists but can't be read, stops the collection.
func Collect(fakeintakeURL string, outcome runner.Outcome) (Capture, error) {
	return collect(client.NewClient(fakeintakeURL), outcome)
}

func collect(c intake, outcome runner.Outcome) (Capture, error) {
	routes, err := c.RouteStats()
	if err != nil {
		return Capture{}, fmt.Errorf("reading the fakeintake route statistics: %w", err)
	}
	result := Capture{Routes: routes, Outcome: outcome, Payloads: []Payload{}}
	names := make([]string, 0, len(routes))
	for route := range routes {
		names = append(names, route)
	}
	sort.Strings(names)
	for _, route := range names {
		raw, err := c.GetRawPayloads(route)
		if err != nil {
			result.Payloads = append(result.Payloads, Payload{Route: route, Error: fmt.Sprintf("reading the payloads: %v", err)})
			continue
		}
		for _, p := range raw {
			result.Payloads = append(result.Payloads, decode(route, p))
		}
	}
	if outcome.EventLogPath != "" {
		if _, err := os.Stat(outcome.EventLogPath); err == nil {
			if result.Events, err = events.Read(outcome.EventLogPath); err != nil {
				return Capture{}, fmt.Errorf("reading the event log: %w", err)
			}
		} else if !errors.Is(err, fs.ErrNotExist) {
			return Capture{}, fmt.Errorf("checking the event log: %w", err)
		}
	}
	return result, nil
}

func decode(route string, p api.Payload) Payload {
	out := Payload{Route: route, ArrivedAt: p.Timestamp}
	decodeFunc, known := decoders[route]
	if !known {
		out.Decoded, out.Error = p, "no decoder"
		return out
	}
	decoded, err := decodeFunc(p)
	if err != nil {
		out.Decoded, out.Error = p, fmt.Sprintf("decoding the payload: %v", err)
		return out
	}
	out.Decoded = decoded
	return out
}

// WriteJSON writes the capture as indented JSON to path.
func (c Capture) WriteJSON(path string) error {
	data, err := json.MarshalIndent(c, "", "  ")
	if err != nil {
		return fmt.Errorf("encoding the capture: %w", err)
	}
	return os.WriteFile(path, data, 0o644)
}
