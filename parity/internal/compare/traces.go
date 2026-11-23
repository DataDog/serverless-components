package compare

import (
	"fmt"
	"slices"
	"strconv"
	"strings"

	"github.com/DataDog/serverless-components/parity/internal/normalize"
)

// traceChunk is one chunk with the fields of the payloads that carried it, so that chunks
// batched differently still match.
type traceChunk struct {
	AgentPayload  normalize.TracePayload  `json:"agent_payload"`
	TracerPayload normalize.TracerPayload `json:"tracer_payload"`
	Chunk         normalize.Chunk         `json:"chunk"`
}

// traceItems flattens both sides into chunks, removes the chunks that are equal on both sides,
// and keys the rest by priority and root span.
func traceItems(reference, actual []normalize.TracePayload) (map[string]any, map[string]any) {
	referenceChunks, actualChunks := traceChunks(reference), traceChunks(actual)
	unmatched := map[string]int{}
	for _, c := range actualChunks {
		unmatched[encode(c)]++
	}
	var referenceLeft []traceChunk
	for _, c := range referenceChunks {
		if key := encode(c); unmatched[key] > 0 {
			unmatched[key]--
		} else {
			referenceLeft = append(referenceLeft, c)
		}
	}
	var actualLeft []traceChunk
	for _, c := range actualChunks {
		if key := encode(c); unmatched[key] > 0 {
			unmatched[key]--
			actualLeft = append(actualLeft, c)
		}
	}
	shared := map[string]int{}
	for _, side := range [][]traceChunk{referenceLeft, actualLeft} {
		counts := map[string]int{}
		for _, c := range side {
			counts[chunkName(c)]++
			shared[chunkName(c)] = max(shared[chunkName(c)], counts[chunkName(c)])
		}
	}
	key := func(side []traceChunk) map[string]any {
		items := map[string]any{}
		counts := map[string]int{}
		for _, c := range side {
			name := chunkName(c)
			if shared[name] > 1 {
				items[fmt.Sprintf("%s,index=%d]", strings.TrimSuffix(name, "]"), counts[name])] = c
			} else {
				items[name] = c
			}
			counts[name]++
		}
		return items
	}
	return key(referenceLeft), key(actualLeft)
}

func chunkName(c traceChunk) string {
	var root normalize.Span
	if len(c.Chunk.Spans) > 0 {
		root = c.Chunk.Spans[0]
	}
	return fmt.Sprintf("traces[priority=%d,service=%s,name=%s,resource=%s]", c.Chunk.Priority, root.Service, root.Name, root.Resource)
}

// traceChunks flattens payloads into chunks, in the canonical order of the normalizer.
func traceChunks(payloads []normalize.TracePayload) []traceChunk {
	var chunks []traceChunk
	for _, p := range payloads {
		agent := p
		agent.TracerPayloads = nil
		for _, t := range p.TracerPayloads {
			tracer := t
			tracer.Chunks = nil
			for _, c := range t.Chunks {
				chunks = append(chunks, traceChunk{AgentPayload: agent, TracerPayload: tracer, Chunk: localLabels(c)})
			}
		}
	}
	return chunks
}

// localLabels renumbers the IDs of a chunk within the chunk, so that an extra or missing chunk
// elsewhere does not shift them. The normalizer has put the spans in tree order. Span IDs become
// span-<position>, parents outside the chunk outside-N and trace IDs trace-N, each numbered by
// first appearance. Span links keep the normalizer's labels, because they point outside the chunk.
func localLabels(c normalize.Chunk) normalize.Chunk {
	spans := map[string]string{}
	for i, s := range c.Spans {
		if s.SpanID != "" {
			if _, ok := spans[s.SpanID]; !ok {
				spans[s.SpanID] = "span-" + strconv.Itoa(i+1)
			}
		}
	}
	outside, traces := map[string]string{}, map[string]string{}
	label := func(labels map[string]string, prefix, id string) string {
		if id == "" {
			return ""
		}
		if _, ok := labels[id]; !ok {
			labels[id] = prefix + strconv.Itoa(len(labels)+1)
		}
		return labels[id]
	}
	c.Spans = slices.Clone(c.Spans)
	for i, s := range c.Spans {
		s.SpanID = spans[s.SpanID]
		if local, ok := spans[s.ParentID]; ok {
			s.ParentID = local
		} else {
			s.ParentID = label(outside, "outside-", s.ParentID)
		}
		s.TraceID = label(traces, "trace-", s.TraceID)
		c.Spans[i] = s
	}
	return c
}
