package normalize

import (
	"cmp"
	"encoding/json"
	"slices"
	"strings"
)

// canonicalTraces orders trace content by what it holds, not by arrival or by ID, so relabel
// rules number trace and span IDs the same way for two runs that sent the same traces. Apply
// runs it after the mask and placeholder rules, so run-specific values do not affect the order.
//   - Spans in each chunk are in tree order: roots first, then each span's children. Siblings
//     are ordered by the content of their whole subtree.
//   - Chunks are ordered by their root span's service, name and resource, then span count, then
//     the content of the chunk and its span tree.
//   - Tracer payloads and agent payloads are ordered by their content and their chunks' keys.
//
// Content leaves out IDs and absolute times, which differ between runs.
func canonicalTraces(payloads []TracePayload) {
	payloadKeys := make([]string, len(payloads))
	for i := range payloads {
		payload := &payloads[i]
		tracerKeys := make([]string, len(payload.TracerPayloads))
		for j := range payload.TracerPayloads {
			tracer := &payload.TracerPayloads[j]
			type keyed struct {
				chunk Chunk
				key   string
			}
			chunks := make([]keyed, len(tracer.Chunks))
			for k, chunk := range tracer.Chunks {
				spans, tree := treeOrder(chunk.Spans)
				chunk.Spans = spans
				own := chunk
				own.Spans = nil
				chunks[k] = keyed{chunk, contentKey(own) + tree}
			}
			slices.SortStableFunc(chunks, func(a, b keyed) int {
				return cmp.Or(compareRoots(a.chunk, b.chunk), strings.Compare(a.key, b.key))
			})
			keys := make([]string, len(chunks))
			for k, c := range chunks {
				tracer.Chunks[k], keys[k] = c.chunk, c.key
			}
			own := *tracer
			own.Chunks = nil
			tracerKeys[j] = contentKey(own) + strings.Join(keys, ",")
		}
		sortByKeys(payload.TracerPayloads, tracerKeys)
		slices.Sort(tracerKeys)
		own := *payload
		own.TracerPayloads = nil
		payloadKeys[i] = contentKey(own) + strings.Join(tracerKeys, ",")
	}
	sortByKeys(payloads, payloadKeys)
}

// sortByKeys sorts items by the matching keys, keeping the order of equal keys.
func sortByKeys[T any](items []T, keys []string) {
	order := make([]int, len(items))
	for i := range order {
		order[i] = i
	}
	slices.SortStableFunc(order, func(a, b int) int { return strings.Compare(keys[a], keys[b]) })
	sorted := make([]T, len(items))
	for i, index := range order {
		sorted[i] = items[index]
	}
	copy(items, sorted)
}

func compareRoots(a, b Chunk) int {
	var rootA, rootB Span
	if len(a.Spans) > 0 {
		rootA = a.Spans[0]
	}
	if len(b.Spans) > 0 {
		rootB = b.Spans[0]
	}
	return cmp.Or(
		strings.Compare(rootA.Service, rootB.Service),
		strings.Compare(rootA.Name, rootB.Name),
		strings.Compare(rootA.Resource, rootB.Resource),
		cmp.Compare(len(a.Spans), len(b.Spans)),
	)
}

// treeOrder returns spans with roots first and each span followed by its children, and a key for
// the whole span tree. A root is a span whose parent is not in the chunk. Siblings are ordered by
// the content of their subtree. Spans that no root reaches, which only a parent cycle causes,
// come last.
func treeOrder(spans []Span) ([]Span, string) {
	present := map[string]bool{}
	for _, s := range spans {
		present[s.SpanID] = true
	}
	children := map[string][]int{}
	var roots []int
	for i, s := range spans {
		if s.ParentID == "" || !present[s.ParentID] || s.ParentID == s.SpanID {
			roots = append(roots, i)
			continue
		}
		children[s.ParentID] = append(children[s.ParentID], i)
	}
	keys := make([]string, len(spans))
	state := make([]int8, len(spans)) // 0 not started, 1 in progress, 2 done
	var subtree func(i int) string
	subtree = func(i int) string {
		switch state[i] {
		case 1:
			return "" // a parent cycle
		case 2:
			return keys[i]
		}
		state[i] = 1
		var kids []string
		if spans[i].SpanID != "" {
			for _, child := range children[spans[i].SpanID] {
				kids = append(kids, subtree(child))
			}
		}
		slices.Sort(kids)
		keys[i] = contentKey(spans[i]) + "[" + strings.Join(kids, ",") + "]"
		state[i] = 2
		return keys[i]
	}
	bySubtree := func(a, b int) int { return strings.Compare(subtree(a), subtree(b)) }
	out := make([]Span, 0, len(spans))
	visited := make([]bool, len(spans))
	var visit func(level []int)
	visit = func(level []int) {
		slices.SortStableFunc(level, bySubtree)
		for _, i := range level {
			if visited[i] {
				continue
			}
			visited[i] = true
			out = append(out, spans[i])
			if spans[i].SpanID != "" {
				visit(children[spans[i].SpanID])
			}
		}
	}
	visit(roots)
	var rest []int
	for i := range spans {
		if !visited[i] {
			rest = append(rest, i)
		}
	}
	slices.SortStableFunc(rest, bySubtree)
	for _, i := range rest {
		out = append(out, spans[i])
	}
	var tree []string
	for _, i := range roots {
		tree = append(tree, subtree(i))
	}
	for _, i := range rest {
		tree = append(tree, subtree(i))
	}
	slices.Sort(tree)
	return out, strings.Join(tree, ",")
}

// runSpecific are the JSON keys that contentKey leaves out.
var runSpecific = map[string]bool{
	"trace_id": true, "span_id": true, "parent_id": true, "trace_id_high": true,
	"start": true, "time_unix_nano": true,
}

// contentKey is the JSON form of v without IDs and absolute times.
func contentKey(v any) string {
	data, _ := json.Marshal(v)
	var tree any
	_ = json.Unmarshal(data, &tree)
	data, _ = json.Marshal(withoutKeys(tree))
	return string(data)
}

func withoutKeys(node any) any {
	switch n := node.(type) {
	case map[string]any:
		for key, value := range n {
			if runSpecific[key] {
				delete(n, key)
				continue
			}
			n[key] = withoutKeys(value)
		}
	case []any:
		for i, value := range n {
			n[i] = withoutKeys(value)
		}
	}
	return node
}
