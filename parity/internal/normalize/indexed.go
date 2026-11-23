package normalize

import (
	"encoding/base64"
	"encoding/binary"
	"fmt"
	"strconv"

	"github.com/DataDog/datadog-agent/pkg/proto/pbgo/trace/idx"
)

// The Go agent sends tracer payloads in the indexed (v1) format by default: strings are
// references into one string table per payload, and meta and metrics are one typed attribute
// map. indexedTracerView turns such a payload into the legacy form of the view, so that a binary
// that sends legacy payloads compares in the same form. It reverses the agent's conversion
// (pkg/trace/api/converter.go ConvertToIdx):
//   - string attributes become meta, and boolean attributes meta "true" or "false"; numeric
//     attributes become metrics, and bytes attributes meta_struct
//   - the promoted env, version and component fields become the meta keys of those names
//   - span.kind comes from its attribute when present, else from the kind field, except the
//     internal kind, which the agent also gives spans that had no span.kind
//   - the chunk's 128-bit trace ID gives every span its low 64 bits as trace_id, and, when the
//     high bits are not zero and no span has it, the first span the meta _dd.p.tid
//   - a non-zero sampling mechanism becomes the chunk tag _dd.p.dm, as -<mechanism>
//
// Anything without a legacy form, such as array attributes, is an error, and the caller lists
// the whole tracer payload as undecoded.
func indexedTracerView(tp *idx.TracerPayload) (TracerPayload, error) {
	r := &resolver{strings: tp.GetStrings()}
	out := TracerPayload{
		ContainerID: r.get(tp.ContainerIDRef), LanguageName: r.get(tp.LanguageNameRef), LanguageVersion: r.get(tp.LanguageVersionRef),
		TracerVersion: r.get(tp.TracerVersionRef), RuntimeID: r.get(tp.RuntimeIDRef), Env: r.get(tp.EnvRef),
		Hostname: r.get(tp.HostnameRef), AppVersion: r.get(tp.AppVersionRef), Chunks: []Chunk{},
	}
	var err error
	if out.Tags, err = r.stringAttributes("tracer payload", tp.Attributes); err != nil {
		return TracerPayload{}, err
	}
	if d := tp.ContainerDebug; d != nil {
		out.ContainerDebug = &ContainerDebug{Error: r.get(d.ErrorRef), LatencyMs: d.LatencyMs, WasBuffered: d.WasBuffered, BufferMs: d.BufferMs, BufferEvictionReason: r.get(d.BufferEvictionReasonRef)}
	}
	for _, c := range tp.Chunks {
		chunk, err := r.chunk(c)
		if err != nil {
			return TracerPayload{}, err
		}
		out.Chunks = append(out.Chunks, chunk)
	}
	return out, r.err
}

// resolver looks up string references and keeps the first reference that was out of range.
type resolver struct {
	strings []string
	err     error
}

// get returns the string of ref. Reference 0 is the empty string, even in an empty table.
func (r *resolver) get(ref uint32) string {
	if ref == 0 {
		return ""
	}
	if int(ref) < len(r.strings) {
		return r.strings[ref]
	}
	if r.err == nil {
		r.err = fmt.Errorf("string reference %d is outside the string table of %d strings", ref, len(r.strings))
	}
	return ""
}

// stringAttributes returns attributes that must all be strings, as legacy tags. It returns nil
// for none.
func (r *resolver) stringAttributes(owner string, attrs map[uint32]*idx.AnyValue) (map[string]string, error) {
	var out map[string]string
	for key, value := range attrs {
		ref, ok := value.GetValue().(*idx.AnyValue_StringValueRef)
		if !ok {
			return nil, fmt.Errorf("%s: attribute %q has a %T value, but legacy tags are strings", owner, r.get(key), value.GetValue())
		}
		if out == nil {
			out = map[string]string{}
		}
		out[r.get(key)] = r.get(ref.StringValueRef)
	}
	return out, nil
}

func (r *resolver) chunk(c *idx.TraceChunk) (Chunk, error) {
	if len(c.TraceID) != 16 {
		return Chunk{}, fmt.Errorf("indexed chunk: trace ID of %d bytes, want 16", len(c.TraceID))
	}
	high, low := binary.BigEndian.Uint64(c.TraceID[:8]), binary.BigEndian.Uint64(c.TraceID[8:])
	out := Chunk{Priority: c.Priority, Origin: r.get(c.OriginRef), DroppedTrace: c.DroppedTrace, Spans: []Span{}}
	var err error
	if out.Tags, err = r.stringAttributes("indexed chunk", c.Attributes); err != nil {
		return Chunk{}, err
	}
	if c.SamplingMechanism != 0 {
		if _, ok := out.Tags["_dd.p.dm"]; !ok {
			if out.Tags == nil {
				out.Tags = map[string]string{}
			}
			out.Tags["_dd.p.dm"] = "-" + strconv.FormatUint(uint64(c.SamplingMechanism), 10)
		}
	}
	hasTID := false
	for _, s := range c.Spans {
		span, err := r.span(s, low)
		if err != nil {
			return Chunk{}, err
		}
		_, found := span.Meta["_dd.p.tid"]
		hasTID = hasTID || found
		out.Spans = append(out.Spans, span)
	}
	if high != 0 && !hasTID && len(out.Spans) > 0 {
		if out.Spans[0].Meta == nil {
			out.Spans[0].Meta = map[string]string{}
		}
		out.Spans[0].Meta["_dd.p.tid"] = fmt.Sprintf("%016x", high)
	}
	return out, nil
}

func (r *resolver) span(s *idx.Span, traceID uint64) (Span, error) {
	out := Span{
		Service: r.get(s.ServiceRef), Name: r.get(s.NameRef), Resource: r.get(s.ResourceRef), Type: r.get(s.TypeRef),
		TraceID: identifier(traceID), SpanID: identifier(s.SpanID), ParentID: identifier(s.ParentID),
		Start: int64(s.Start), Duration: int64(s.Duration),
	}
	if s.Error {
		out.Error = 1
	}
	setMeta := func(key, value string) {
		if out.Meta == nil {
			out.Meta = map[string]string{}
		}
		out.Meta[key] = value
	}
	for key, value := range s.Attributes {
		name := r.get(key)
		switch v := value.GetValue().(type) {
		case *idx.AnyValue_StringValueRef:
			setMeta(name, r.get(v.StringValueRef))
		case *idx.AnyValue_BoolValue:
			setMeta(name, strconv.FormatBool(v.BoolValue))
		case *idx.AnyValue_DoubleValue:
			out.setMetric(name, v.DoubleValue)
		case *idx.AnyValue_IntValue:
			out.setMetric(name, float64(v.IntValue))
		case *idx.AnyValue_BytesValue:
			if out.MetaStruct == nil {
				out.MetaStruct = map[string]string{}
			}
			out.MetaStruct[name] = base64.StdEncoding.EncodeToString(v.BytesValue)
		case *idx.AnyValue_ArrayValue:
			return Span{}, fmt.Errorf("indexed span %s: attribute %q has an array value, which a legacy span cannot hold", out.Service, name)
		default:
			return Span{}, fmt.Errorf("indexed span %s: attribute %q has a %T value, which a legacy span cannot hold", out.Service, name, v)
		}
	}
	for _, promoted := range []struct {
		key string
		ref uint32
	}{{"env", s.EnvRef}, {"version", s.VersionRef}, {"component", s.ComponentRef}} {
		if value := r.get(promoted.ref); value != "" {
			setMeta(promoted.key, value)
		}
	}
	if _, ok := out.Meta["span.kind"]; !ok {
		if kind := spanKind(s.Kind); kind != "" {
			setMeta("span.kind", kind)
		}
	}
	for _, l := range s.Links {
		if len(l.TraceID) != 16 {
			return Span{}, fmt.Errorf("indexed span %s: link trace ID of %d bytes, want 16", out.Service, len(l.TraceID))
		}
		attributes, err := r.stringAttributes("indexed span link of "+out.Service, l.Attributes)
		if err != nil {
			return Span{}, err
		}
		out.Links = append(out.Links, SpanLink{
			TraceID: identifier(binary.BigEndian.Uint64(l.TraceID[8:])), TraceIDHigh: identifier(binary.BigEndian.Uint64(l.TraceID[:8])),
			SpanID: identifier(l.SpanID), Attributes: attributes, Tracestate: r.get(l.TracestateRef), Flags: l.Flags,
		})
	}
	for _, e := range s.Events {
		event := SpanEvent{TimeUnixNano: e.Time, Name: r.get(e.NameRef)}
		for key, value := range e.Attributes {
			converted, err := r.eventAttribute(value, true)
			if err != nil {
				return Span{}, fmt.Errorf("indexed span %s: event %q attribute %q: %w", out.Service, event.Name, r.get(key), err)
			}
			if event.Attributes == nil {
				event.Attributes = map[string]AttributeValue{}
			}
			event.Attributes[r.get(key)] = converted
		}
		out.Events = append(out.Events, event)
	}
	return out, nil
}

func (s *Span) setMetric(key string, value float64) {
	if s.Metrics == nil {
		s.Metrics = map[string]float64{}
	}
	s.Metrics[key] = value
}

// eventAttribute converts a span event attribute. Arrays hold only scalar values.
func (r *resolver) eventAttribute(value *idx.AnyValue, arrayAllowed bool) (AttributeValue, error) {
	switch v := value.GetValue().(type) {
	case *idx.AnyValue_StringValueRef:
		return AttributeValue{Type: "string", String: r.get(v.StringValueRef)}, nil
	case *idx.AnyValue_BoolValue:
		return AttributeValue{Type: "bool", Bool: v.BoolValue}, nil
	case *idx.AnyValue_IntValue:
		return AttributeValue{Type: "int", Int: v.IntValue}, nil
	case *idx.AnyValue_DoubleValue:
		return AttributeValue{Type: "double", Double: v.DoubleValue}, nil
	case *idx.AnyValue_ArrayValue:
		if arrayAllowed {
			out := AttributeValue{Type: "array"}
			for _, element := range v.ArrayValue.GetValues() {
				converted, err := r.eventAttribute(element, false)
				if err != nil {
					return AttributeValue{}, err
				}
				out.Array = append(out.Array, converted)
			}
			return out, nil
		}
	}
	return AttributeValue{}, fmt.Errorf("a %T value has no legacy span event form", value.GetValue())
}

// spanKind returns the span.kind meta value of an indexed kind. It returns "" for the internal
// kind, which the agent gives every span without a span.kind, and for an unset kind.
func spanKind(kind idx.SpanKind) string {
	switch kind {
	case idx.SpanKind_SPAN_KIND_SERVER:
		return "server"
	case idx.SpanKind_SPAN_KIND_CLIENT:
		return "client"
	case idx.SpanKind_SPAN_KIND_PRODUCER:
		return "producer"
	case idx.SpanKind_SPAN_KIND_CONSUMER:
		return "consumer"
	}
	return ""
}

// indexedResources returns the resource of every span in an indexed tracer payload. A reference
// outside the string table gives the empty resource.
func indexedResources(tp *idx.TracerPayload) []string {
	r := &resolver{strings: tp.GetStrings()}
	var out []string
	for _, c := range tp.Chunks {
		for _, s := range c.Spans {
			out = append(out, r.get(s.ResourceRef))
		}
	}
	return out
}
