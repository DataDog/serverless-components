package normalize

import (
	"encoding/base64"
	"fmt"
	"strconv"
	"strings"

	metricspb "github.com/DataDog/agent-payload/v5/gogen"
	pb "github.com/DataDog/datadog-agent/pkg/proto/pbgo/trace"
	"github.com/DataDog/datadog-agent/test/fakeintake/aggregator"
	"github.com/DataDog/sketches-go/ddsketch"
	"github.com/DataDog/sketches-go/ddsketch/pb/sketchpb"
	"google.golang.org/protobuf/proto"

	"github.com/DataDog/serverless-components/parity/internal/capture"
	"github.com/DataDog/serverless-components/parity/internal/timing"
)

// Normalized is the comparable form of one capture. Each content area lists the items of every
// payload of its route, in arrival order, except Traces, which canonicalTraces orders by content
// after the mask and placeholder rules.
// Field names follow the JSON names, which rule paths use. Identifiers are strings so that
// relabel rules can replace them. Fields a rule drops are left at their zero value, and omitted
// from the JSON form.
type Normalized struct {
	Series   []Series       `json:"series"`
	Sketches []Sketch       `json:"sketches"`
	Logs     []Log          `json:"logs"`
	Traces   []TracePayload `json:"traces"`
	Stats    []StatsPayload `json:"stats"`
	// CheckRuns are the service checks from /api/v1/check_run.
	CheckRuns []CheckRun  `json:"check_runs"`
	Process   ProcessView `json:"process"`
	// Undecoded lists the payloads that could not be decoded, or that came to a route the
	// harness has no decoder for.
	Undecoded []Undecoded  `json:"undecoded,omitempty"`
	Timing    timing.Input `json:"timing"`
	// UnmatchedRules are the rules whose path reached nothing in this capture.
	UnmatchedRules []Rule `json:"-"`
}

// Series is one metric series from /api/v2/series.
type Series struct {
	Metric string `json:"metric"`
	// Type is count, rate, gauge or unspecified.
	Type           string     `json:"type"`
	Interval       int64      `json:"interval,omitempty"`
	Unit           string     `json:"unit,omitempty"`
	SourceTypeName string     `json:"source_type_name,omitempty"`
	Host           string     `json:"host,omitempty"`
	Resources      []Resource `json:"resources,omitempty"`
	Tags           []string   `json:"tags,omitempty"`
	Origin         *Origin    `json:"origin,omitempty"`
	Points         []Point    `json:"points"`
}

// Origin is the origin metadata of a series or sketch, which names the product that produced it.
type Origin struct {
	Product  uint32 `json:"product,omitempty"`
	Category uint32 `json:"category,omitempty"`
	Service  uint32 `json:"service,omitempty"`
}

// Resource is a series resource other than the host, which Series.Host holds.
type Resource struct {
	Type string `json:"type"`
	Name string `json:"name"`
}

// Point is one series value. Timestamp is in seconds.
type Point struct {
	Timestamp Timestamp `json:"timestamp,omitzero"`
	Value     float64   `json:"value"`
}

// Sketch is one distribution from /api/beta/sketches.
type Sketch struct {
	Metric string        `json:"metric"`
	Host   string        `json:"host,omitempty"`
	Tags   []string      `json:"tags,omitempty"`
	Origin *Origin       `json:"origin,omitempty"`
	Points []SketchPoint `json:"points"`
	// Distributions is the older sketch encoding, which the agent normally leaves empty.
	Distributions []Distribution `json:"distributions,omitempty"`
}

// SketchPoint is one sketch interval. K and N are the sketch bins.
type SketchPoint struct {
	Timestamp Timestamp `json:"timestamp,omitzero"`
	Count     int64     `json:"count"`
	Min       float64   `json:"min"`
	Max       float64   `json:"max"`
	Avg       float64   `json:"avg"`
	Sum       float64   `json:"sum"`
	K         []int32   `json:"k,omitempty"`
	N         []uint32  `json:"n,omitempty"`
}

// Distribution is one interval in the older sketch encoding.
type Distribution struct {
	Timestamp Timestamp `json:"timestamp,omitzero"`
	Count     int64     `json:"count"`
	Min       float64   `json:"min"`
	Max       float64   `json:"max"`
	Avg       float64   `json:"avg"`
	Sum       float64   `json:"sum"`
	V         []float64 `json:"v,omitempty"`
	G         []uint32  `json:"g,omitempty"`
	Delta     []uint32  `json:"delta,omitempty"`
	Buf       []float64 `json:"buf,omitempty"`
}

// Log is one log from /api/v2/logs.
type Log struct {
	Message   string    `json:"message"`
	Status    string    `json:"status,omitempty"`
	Timestamp Timestamp `json:"timestamp,omitzero"`
	Hostname  string    `json:"hostname,omitempty"`
	Service   string    `json:"service,omitempty"`
	Source    string    `json:"ddsource,omitempty"`
	Tags      []string  `json:"ddtags,omitempty"`
}

// CheckRun is one service check run. Timestamp is in seconds. Status is 0 (ok), 1 (warning),
// 2 (critical) or 3 (unknown).
type CheckRun struct {
	Check     string    `json:"check"`
	HostName  string    `json:"host_name,omitempty"`
	Timestamp Timestamp `json:"timestamp,omitzero"`
	Status    int       `json:"status"`
	Message   string    `json:"message,omitempty"`
	Tags      []string  `json:"tags,omitempty"`
}

// TracePayload is one agent payload from /api/v0.2/traces.
type TracePayload struct {
	HostName       string            `json:"host_name,omitempty"`
	Env            string            `json:"env,omitempty"`
	AgentVersion   string            `json:"agent_version,omitempty"`
	TargetTPS      float64           `json:"target_tps,omitempty"`
	ErrorTPS       float64           `json:"error_tps,omitempty"`
	RareSampler    bool              `json:"rare_sampler_enabled,omitempty"`
	Tags           map[string]string `json:"tags,omitempty"`
	TracerPayloads []TracerPayload   `json:"tracer_payloads"`
}

// TracerPayload is the part of a trace payload that one tracer sent.
type TracerPayload struct {
	ContainerID     string            `json:"container_id,omitempty"`
	LanguageName    string            `json:"language_name,omitempty"`
	LanguageVersion string            `json:"language_version,omitempty"`
	TracerVersion   string            `json:"tracer_version,omitempty"`
	RuntimeID       string            `json:"runtime_id,omitempty"`
	Env             string            `json:"env,omitempty"`
	Hostname        string            `json:"hostname,omitempty"`
	AppVersion      string            `json:"app_version,omitempty"`
	Tags            map[string]string `json:"tags,omitempty"`
	ContainerDebug  *ContainerDebug   `json:"container_debug,omitempty"`
	Chunks          []Chunk           `json:"chunks"`
}

// ContainerDebug is the tracer's report on how it found the container tags.
type ContainerDebug struct {
	Error                string `json:"error,omitempty"`
	LatencyMs            int64  `json:"latency_ms,omitempty"`
	WasBuffered          bool   `json:"was_buffered,omitempty"`
	BufferMs             int64  `json:"buffer_ms,omitempty"`
	BufferEvictionReason string `json:"buffer_eviction_reason,omitempty"`
}

// Chunk is the spans of one trace.
type Chunk struct {
	Priority     int32             `json:"priority"`
	Origin       string            `json:"origin,omitempty"`
	Tags         map[string]string `json:"tags,omitempty"`
	DroppedTrace bool              `json:"dropped_trace,omitempty"`
	Spans        []Span            `json:"spans"`
}

// Span is one span. ParentID is empty for a root span. Start is in nanoseconds since the epoch.
type Span struct {
	Service  string             `json:"service,omitempty"`
	Name     string             `json:"name,omitempty"`
	Resource string             `json:"resource,omitempty"`
	TraceID  string             `json:"trace_id,omitempty"`
	SpanID   string             `json:"span_id,omitempty"`
	ParentID string             `json:"parent_id,omitempty"`
	Start    int64              `json:"start,omitempty"`
	Duration int64              `json:"duration,omitempty"`
	Error    int32              `json:"error,omitempty"`
	Type     string             `json:"type,omitempty"`
	Meta     map[string]string  `json:"meta,omitempty"`
	Metrics  map[string]float64 `json:"metrics,omitempty"`
	// MetaStruct values are the raw msgpack, base64 encoded.
	MetaStruct map[string]string `json:"meta_struct,omitempty"`
	Links      []SpanLink        `json:"span_links,omitempty"`
	Events     []SpanEvent       `json:"span_events,omitempty"`
}

// SpanLink is a link from a span to another span.
type SpanLink struct {
	TraceID     string            `json:"trace_id,omitempty"`
	TraceIDHigh string            `json:"trace_id_high,omitempty"`
	SpanID      string            `json:"span_id,omitempty"`
	Attributes  map[string]string `json:"attributes,omitempty"`
	Tracestate  string            `json:"tracestate,omitempty"`
	Flags       uint32            `json:"flags,omitempty"`
}

// SpanEvent is an event on a span. TimeUnixNano is in nanoseconds since the epoch.
type SpanEvent struct {
	TimeUnixNano uint64                    `json:"time_unix_nano,omitempty"`
	Name         string                    `json:"name"`
	Attributes   map[string]AttributeValue `json:"attributes,omitempty"`
}

// AttributeValue is a span event attribute. Type is string, bool, int, double or array, and only
// the matching field is set. Array elements have no Array field.
type AttributeValue struct {
	Type   string           `json:"type"`
	String string           `json:"string,omitempty"`
	Bool   bool             `json:"bool,omitempty"`
	Int    int64            `json:"int,omitempty"`
	Double float64          `json:"double,omitempty"`
	Array  []AttributeValue `json:"array,omitempty"`
}

// StatsPayload is one payload from /api/v0.2/stats.
type StatsPayload struct {
	AgentHostname  string        `json:"agent_hostname,omitempty"`
	AgentEnv       string        `json:"agent_env,omitempty"`
	AgentVersion   string        `json:"agent_version,omitempty"`
	ClientComputed bool          `json:"client_computed,omitempty"`
	SplitPayload   bool          `json:"split_payload,omitempty"`
	Stats          []ClientStats `json:"stats"`
}

// ClientStats is the stats of one tracer or of the agent.
type ClientStats struct {
	Hostname      string   `json:"hostname,omitempty"`
	Env           string   `json:"env,omitempty"`
	Version       string   `json:"version,omitempty"`
	Lang          string   `json:"lang,omitempty"`
	TracerVersion string   `json:"tracer_version,omitempty"`
	RuntimeID     string   `json:"runtime_id,omitempty"`
	Sequence      uint64   `json:"sequence,omitempty"`
	Service       string   `json:"service,omitempty"`
	ContainerID   string   `json:"container_id,omitempty"`
	Tags          []string `json:"tags,omitempty"`
	// AgentAggregation, GitCommitSha, ImageTag and the process tags are grouping keys.
	AgentAggregation string        `json:"agent_aggregation,omitempty"`
	GitCommitSha     string        `json:"git_commit_sha,omitempty"`
	ImageTag         string        `json:"image_tag,omitempty"`
	ProcessTagsHash  uint64        `json:"process_tags_hash,omitempty"`
	ProcessTags      string        `json:"process_tags,omitempty"`
	Buckets          []StatsBucket `json:"buckets"`
}

// StatsBucket is one time bucket. Start and Duration are in nanoseconds.
type StatsBucket struct {
	Start          Timestamp      `json:"start,omitzero"`
	Duration       uint64         `json:"duration,omitempty"`
	AgentTimeShift int64          `json:"agent_time_shift,omitempty"`
	Stats          []GroupedStats `json:"stats"`
}

// GroupedStats is the stats of one group of spans. OkSummary and ErrorSummary are the decoded
// latency sketches of the spans without and with an error.
type GroupedStats struct {
	Service                string   `json:"service,omitempty"`
	Name                   string   `json:"name,omitempty"`
	Resource               string   `json:"resource,omitempty"`
	HTTPStatusCode         uint32   `json:"http_status_code,omitempty"`
	HTTPMethod             string   `json:"http_method,omitempty"`
	HTTPEndpoint           string   `json:"http_endpoint,omitempty"`
	GRPCStatusCode         string   `json:"grpc_status_code,omitempty"`
	Type                   string   `json:"type,omitempty"`
	DBType                 string   `json:"db_type,omitempty"`
	SpanKind               string   `json:"span_kind,omitempty"`
	ServiceSource          string   `json:"service_source,omitempty"`
	PeerTags               []string `json:"peer_tags,omitempty"`
	SpanDerivedPrimaryTags []string `json:"span_derived_primary_tags,omitempty"`
	AdditionalMetricTags   []string `json:"additional_metric_tags,omitempty"`
	IsTraceRoot            string   `json:"is_trace_root,omitempty"`
	Synthetics             bool     `json:"synthetics,omitempty"`
	Hits                   uint64   `json:"hits,omitempty"`
	Errors                 uint64   `json:"errors,omitempty"`
	TopLevelHits           uint64   `json:"top_level_hits,omitempty"`
	Duration               uint64   `json:"duration,omitempty"`
	OkSummary              *Summary `json:"ok_summary,omitempty"`
	ErrorSummary           *Summary `json:"error_summary,omitempty"`
}

// Summary is a decoded latency sketch (a DDSketch), kept whole so that the summaries of several
// buckets can be merged exactly. Gamma, IndexOffset and Interpolation are the index mapping.
// Positive and Negative map a bin index to its count, for positive values and for the magnitude
// of negative values. ZeroCount counts the values mapped to zero.
type Summary struct {
	Gamma         float64           `json:"gamma"`
	IndexOffset   float64           `json:"index_offset"`
	Interpolation string            `json:"interpolation,omitempty"`
	Positive      map[int32]float64 `json:"positive,omitempty"`
	Negative      map[int32]float64 `json:"negative,omitempty"`
	ZeroCount     float64           `json:"zero_count"`
}

// decodeSummary decodes a protobuf DDSketch. It returns nil for no bytes.
func decodeSummary(raw []byte) (*Summary, error) {
	if len(raw) == 0 {
		return nil, nil
	}
	var message sketchpb.DDSketch
	if err := proto.Unmarshal(raw, &message); err != nil {
		return nil, err
	}
	if _, err := ddsketch.FromProto(&message); err != nil {
		return nil, err
	}
	mapping := message.GetMapping()
	out := &Summary{
		Gamma: mapping.GetGamma(), IndexOffset: mapping.GetIndexOffset(),
		Positive: storeBins(message.GetPositiveValues()), Negative: storeBins(message.GetNegativeValues()),
		ZeroCount: message.GetZeroCount(),
	}
	if mapping.GetInterpolation() != sketchpb.IndexMapping_NONE {
		out.Interpolation = mapping.GetInterpolation().String()
	}
	return out, nil
}

// storeBins returns the bins of a sketch store, sparse and contiguous, as one map. It leaves out
// empty bins, and returns nil when there are none.
func storeBins(s *sketchpb.Store) map[int32]float64 {
	bins := map[int32]float64{}
	for index, count := range s.GetBinCounts() {
		bins[index] += count
	}
	for i, count := range s.GetContiguousBinCounts() {
		bins[s.GetContiguousBinIndexOffset()+int32(i)] += count
	}
	for index, count := range bins {
		if count == 0 {
			delete(bins, index)
		}
	}
	if len(bins) == 0 {
		return nil
	}
	return bins
}

// ProcessView is what the workload driver, as the agent's child, saw and printed.
type ProcessView struct {
	ExitCode int `json:"exit_code"`
	// Arguments are the driver's arguments, including the program name.
	Arguments []string `json:"arguments,omitempty"`
	// Signals are the signals the driver received, in order.
	Signals []string `json:"signals,omitempty"`
	// Environment is the driver's DD_* environment.
	Environment map[string]string `json:"environment,omitempty"`
	// Stdout and Stderr are the agent's output lines that start with the child marker, in order.
	Stdout []string `json:"stdout,omitempty"`
	Stderr []string `json:"stderr,omitempty"`
	// NotExited says how long the agent outlived its deadline, when it did.
	NotExited string `json:"not_exited,omitempty"`
}

// Undecoded is a payload that Normalized has no content for.
type Undecoded struct {
	Route string `json:"route"`
	Error string `json:"error"`
}

// view builds the normalized form of c before any rule applies.
func view(c capture.Capture) (Normalized, error) {
	var n Normalized
	for _, p := range c.Payloads {
		if p.Error != "" {
			n.Undecoded = append(n.Undecoded, Undecoded{Route: p.Route, Error: p.Error})
			continue
		}
		switch decoded := p.Decoded.(type) {
		case []*aggregator.MetricSeries:
			for _, s := range decoded {
				n.Series = append(n.Series, seriesView(s))
			}
		case []*aggregator.Sketch:
			for _, s := range decoded {
				n.Sketches = append(n.Sketches, sketchView(s))
			}
		case []*aggregator.Log:
			for _, l := range decoded {
				n.Logs = append(n.Logs, logView(l))
			}
		case []*aggregator.TracePayload:
			for _, t := range decoded {
				if t.AgentPayload == nil {
					n.Undecoded = append(n.Undecoded, Undecoded{Route: p.Route, Error: "the trace payload has no agent payload"})
					continue
				}
				trace, errs := traceView(t)
				n.Traces = append(n.Traces, trace)
				for _, err := range errs {
					n.Undecoded = append(n.Undecoded, Undecoded{Route: p.Route, Error: err.Error()})
				}
			}
		case []*aggregator.APMStatsPayload:
			for _, s := range decoded {
				if s.StatsPayload == nil {
					n.Undecoded = append(n.Undecoded, Undecoded{Route: p.Route, Error: "the stats payload has no content"})
					continue
				}
				stats, errs := statsView(s)
				n.Stats = append(n.Stats, stats)
				for _, err := range errs {
					n.Undecoded = append(n.Undecoded, Undecoded{Route: p.Route, Error: err.Error()})
				}
			}
		case []*aggregator.CheckRun:
			for _, r := range decoded {
				n.CheckRuns = append(n.CheckRuns, CheckRun{
					Check: r.Check, HostName: r.HostName, Timestamp: Timestamp{Value: int64(r.Timestamp)}, Status: r.Status, Message: r.Message, Tags: r.Tags,
				})
			}
		default:
			return Normalized{}, fmt.Errorf("payload on route %s has the unexpected type %T", p.Route, p.Decoded)
		}
	}
	n.Process = processView(c)
	return n, nil
}

func seriesView(s *aggregator.MetricSeries) Series {
	out := Series{
		Metric: s.Metric, Type: strings.ToLower(s.Type.String()), Interval: s.Interval, Unit: s.Unit,
		SourceTypeName: s.SourceTypeName, Tags: s.Tags, Origin: originView(s.Metadata), Points: []Point{},
	}
	for _, r := range s.Resources {
		if r.Type == "host" {
			out.Host = r.Name
			continue
		}
		out.Resources = append(out.Resources, Resource{Type: r.Type, Name: r.Name})
	}
	for _, p := range s.Points {
		out.Points = append(out.Points, Point{Timestamp: Timestamp{Value: p.Timestamp}, Value: p.Value})
	}
	return out
}

func sketchView(s *aggregator.Sketch) Sketch {
	out := Sketch{Metric: s.Metric, Host: s.Host, Tags: s.Tags, Origin: originView(s.Metadata), Points: []SketchPoint{}}
	for _, d := range s.Dogsketches {
		out.Points = append(out.Points, SketchPoint{Timestamp: Timestamp{Value: d.Ts}, Count: d.Cnt, Min: d.Min, Max: d.Max, Avg: d.Avg, Sum: d.Sum, K: d.K, N: d.N})
	}
	for _, d := range s.Distributions {
		out.Distributions = append(out.Distributions, Distribution{
			Timestamp: Timestamp{Value: d.Ts}, Count: d.Cnt, Min: d.Min, Max: d.Max, Avg: d.Avg, Sum: d.Sum, V: d.V, G: d.G, Delta: d.Delta, Buf: d.Buf,
		})
	}
	return out
}

func originView(m *metricspb.Metadata) *Origin {
	if m == nil || m.Origin == nil {
		return nil
	}
	return &Origin{Product: m.Origin.OriginProduct, Category: m.Origin.OriginCategory, Service: m.Origin.OriginService}
}

func logView(l *aggregator.Log) Log {
	out := Log{Message: l.Message, Status: l.Status, Timestamp: Timestamp{Value: int64(l.Timestamp)}, Hostname: l.HostName, Service: l.Service, Source: l.Source}
	for _, tag := range l.Tags {
		if tag != "" {
			out.Tags = append(out.Tags, tag)
		}
	}
	return out
}

// traceView also returns the errors of the indexed tracer payloads it could not convert, which
// it leaves out. t must have an agent payload.
func traceView(t *aggregator.TracePayload) (TracePayload, []error) {
	out := TracePayload{TracerPayloads: []TracerPayload{}}

	out.HostName, out.Env, out.AgentVersion, out.Tags = t.HostName, t.Env, t.AgentVersion, t.Tags
	out.TargetTPS, out.ErrorTPS, out.RareSampler = t.TargetTPS, t.ErrorTPS, t.RareSamplerEnabled
	for _, tp := range t.TracerPayloads {
		tracer := TracerPayload{
			ContainerID: tp.ContainerID, LanguageName: tp.LanguageName, LanguageVersion: tp.LanguageVersion,
			TracerVersion: tp.TracerVersion, RuntimeID: tp.RuntimeID, Env: tp.Env, Hostname: tp.Hostname,
			AppVersion: tp.AppVersion, Tags: tp.Tags, Chunks: []Chunk{},
		}
		if d := tp.ContainerDebug; d != nil {
			tracer.ContainerDebug = &ContainerDebug{Error: d.Error, LatencyMs: d.LatencyMs, WasBuffered: d.WasBuffered, BufferMs: d.BufferMs, BufferEvictionReason: d.BufferEvictionReason}
		}
		for _, c := range tp.Chunks {
			chunk := Chunk{Priority: c.Priority, Origin: c.Origin, Tags: c.Tags, DroppedTrace: c.DroppedTrace, Spans: []Span{}}
			for _, s := range c.Spans {
				chunk.Spans = append(chunk.Spans, spanView(s))
			}
			tracer.Chunks = append(tracer.Chunks, chunk)
		}
		out.TracerPayloads = append(out.TracerPayloads, tracer)
	}
	var errs []error
	for _, tp := range t.IdxTracerPayloads {
		tracer, err := indexedTracerView(tp)
		if err != nil {
			errs = append(errs, err)
			continue
		}
		out.TracerPayloads = append(out.TracerPayloads, tracer)
	}
	return out, errs
}

func spanView(s *pb.Span) Span {
	out := Span{
		Service: s.Service, Name: s.Name, Resource: s.Resource,
		TraceID: identifier(s.TraceID), SpanID: identifier(s.SpanID), ParentID: identifier(s.ParentID),
		Start: s.Start, Duration: s.Duration, Error: s.Error, Type: s.Type, Meta: s.Meta, Metrics: s.Metrics,
	}
	for key, value := range s.MetaStruct {
		if out.MetaStruct == nil {
			out.MetaStruct = map[string]string{}
		}
		out.MetaStruct[key] = base64.StdEncoding.EncodeToString(value)
	}
	for _, l := range s.SpanLinks {
		out.Links = append(out.Links, SpanLink{
			TraceID: identifier(l.TraceID), TraceIDHigh: identifier(l.TraceIDHigh), SpanID: identifier(l.SpanID),
			Attributes: l.Attributes, Tracestate: l.Tracestate, Flags: l.Flags,
		})
	}
	for _, e := range s.SpanEvents {
		event := SpanEvent{TimeUnixNano: e.TimeUnixNano, Name: e.Name}
		for key, value := range e.Attributes {
			if event.Attributes == nil {
				event.Attributes = map[string]AttributeValue{}
			}
			event.Attributes[key] = attributeView(value)
		}
		out.Events = append(out.Events, event)
	}
	return out
}

func attributeView(v *pb.AttributeAnyValue) AttributeValue {
	if v == nil {
		return AttributeValue{}
	}
	out := AttributeValue{Type: attributeType(v.Type.String()), String: v.StringValue, Bool: v.BoolValue, Int: v.IntValue, Double: v.DoubleValue}
	if v.ArrayValue != nil {
		for _, element := range v.ArrayValue.Values {
			out.Array = append(out.Array, AttributeValue{
				Type: attributeType(element.Type.String()), String: element.StringValue, Bool: element.BoolValue, Int: element.IntValue, Double: element.DoubleValue,
			})
		}
	}
	return out
}

// attributeType turns STRING_VALUE into string.
func attributeType(name string) string {
	return strings.ToLower(strings.TrimSuffix(name, "_VALUE"))
}

// identifier formats a span or trace ID, with the empty string for zero, which means none.
func identifier(id uint64) string {
	if id == 0 {
		return ""
	}
	return strconv.FormatUint(id, 10)
}

// statsView also returns the errors of the summaries it could not decode. Their groups keep a
// nil summary. s must have a stats payload.
func statsView(s *aggregator.APMStatsPayload) (StatsPayload, []error) {
	out := StatsPayload{Stats: []ClientStats{}}
	var errs []error
	summary := func(which string, g *pb.ClientGroupedStats, raw []byte) *Summary {
		decoded, err := decodeSummary(raw)
		if err != nil {
			errs = append(errs, fmt.Errorf("decoding the %s summary of %s %s %s: %w", which, g.Service, g.Name, g.Resource, err))
		}
		return decoded
	}
	out.AgentHostname, out.AgentEnv, out.AgentVersion = s.AgentHostname, s.AgentEnv, s.AgentVersion
	out.ClientComputed, out.SplitPayload = s.ClientComputed, s.SplitPayload
	for _, cs := range s.Stats {
		client := ClientStats{
			Hostname: cs.Hostname, Env: cs.Env, Version: cs.Version, Lang: cs.Lang, TracerVersion: cs.TracerVersion,
			RuntimeID: cs.RuntimeID, Sequence: cs.Sequence, Service: cs.Service, ContainerID: cs.ContainerID,
			Tags: cs.Tags, AgentAggregation: cs.AgentAggregation, GitCommitSha: cs.GitCommitSha, ImageTag: cs.ImageTag,
			ProcessTagsHash: cs.ProcessTagsHash, ProcessTags: cs.ProcessTags, Buckets: []StatsBucket{},
		}
		for _, b := range cs.Stats {
			bucket := StatsBucket{Start: Timestamp{Value: int64(b.Start)}, Duration: b.Duration, AgentTimeShift: b.AgentTimeShift, Stats: []GroupedStats{}}
			for _, g := range b.Stats {
				bucket.Stats = append(bucket.Stats, GroupedStats{
					Service: g.Service, Name: g.Name, Resource: g.Resource, HTTPStatusCode: g.HTTPStatusCode,
					HTTPMethod: g.HTTPMethod, HTTPEndpoint: g.HTTPEndpoint, GRPCStatusCode: g.GRPCStatusCode,
					Type: g.Type, DBType: g.DBType, SpanKind: g.SpanKind, ServiceSource: g.ServiceSource,
					PeerTags: g.PeerTags, SpanDerivedPrimaryTags: g.SpanDerivedPrimaryTags, AdditionalMetricTags: g.AdditionalMetricTags,
					IsTraceRoot: traceRoot(g.IsTraceRoot.String()), Synthetics: g.Synthetics,
					Hits: g.Hits, Errors: g.Errors, TopLevelHits: g.TopLevelHits, Duration: g.Duration,
					OkSummary: summary("ok", g, g.OkSummary), ErrorSummary: summary("error", g, g.ErrorSummary),
				})
			}
			client.Buckets = append(client.Buckets, bucket)
		}
		out.Stats = append(out.Stats, client)
	}
	return out, errs
}

// traceRoot leaves out the default NOT_SET value.
func traceRoot(value string) string {
	if value == "NOT_SET" {
		return ""
	}
	return value
}

func processView(c capture.Capture) ProcessView {
	view := ProcessView{
		ExitCode:  c.Outcome.ExitCode,
		Stdout:    childLines(c.Outcome.Stdout),
		Stderr:    childLines(c.Outcome.Stderr),
		NotExited: c.Outcome.NotExited,
	}
	for _, e := range c.Events {
		switch e.Kind {
		case "started":
			if view.Arguments != nil || view.Environment != nil {
				continue
			}
			view.Arguments = stringList(e.Detail["arguments"])
			environment, _ := e.Detail["environment"].(map[string]any)
			for key, value := range environment {
				if strings.HasPrefix(key, "DD_") {
					if view.Environment == nil {
						view.Environment = map[string]string{}
					}
					view.Environment[key] = fmt.Sprint(value)
				}
			}
		case "signal":
			if name, ok := e.Detail["signal"].(string); ok {
				view.Signals = append(view.Signals, name)
			}
		}
	}
	return view
}

func childLines(output string) []string {
	var out []string
	for _, line := range strings.Split(output, "\n") {
		line = strings.TrimSuffix(line, "\r")
		if strings.HasPrefix(line, childMarker) {
			out = append(out, line)
		}
	}
	return out
}

// stringList reads a list from an event detail, which is []any after the event log is read back.
func stringList(v any) []string {
	switch list := v.(type) {
	case []string:
		return list
	case []any:
		out := make([]string, len(list))
		for i, item := range list {
			out[i] = fmt.Sprint(item)
		}
		return out
	}
	return nil
}
