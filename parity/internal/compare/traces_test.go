package compare

import (
	"reflect"
	"testing"

	"github.com/DataDog/serverless-components/parity/internal/diff"
	"github.com/DataDog/serverless-components/parity/internal/normalize"
)

// chunk is a root span "GET /" with one child, whose resource is childResource.
func chunk(priority int32, traceID, rootID, childID, childParent, childResource string) normalize.Chunk {
	return normalize.Chunk{Priority: priority, Spans: []normalize.Span{
		{Service: "web", Name: "http.request", Resource: "GET /", TraceID: traceID, SpanID: rootID},
		{Service: "web", Name: "db.query", Resource: childResource, TraceID: traceID, SpanID: childID, ParentID: childParent},
	}}
}

func traces(chunks ...normalize.Chunk) normalize.Normalized {
	return normalize.Normalized{Traces: []normalize.TracePayload{{
		Env:            "test",
		TracerPayloads: []normalize.TracerPayload{{LanguageName: "python", Chunks: chunks}},
	}}}
}

func TestTracesCompareStructureNotIDs(t *testing.T) {
	const name = "traces[priority=1,service=web,name=http.request,resource=GET /]"
	reference := traces(chunk(1, "trace-1", "span-1", "span-2", "span-1", "SELECT 1"))
	for _, tc := range []struct {
		name   string
		actual normalize.Normalized
		want   []diff.Difference
	}{
		{"different labels, same structure", traces(chunk(1, "trace-4", "span-7", "span-8", "span-7", "SELECT 1")), nil},
		{"child without its parent", traces(chunk(1, "trace-4", "span-7", "span-8", "", "SELECT 1")),
			[]diff.Difference{{Area: "traces", Path: name + ".chunk.spans[1].parent_id", Reference: "span-1", Actual: ""}}},
		{"child with a parent outside the chunk", traces(chunk(1, "trace-4", "span-7", "span-8", "span-3", "SELECT 1")),
			[]diff.Difference{{Area: "traces", Path: name + ".chunk.spans[1].parent_id", Reference: "span-1", Actual: "outside-1"}}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			checkDifferences(t, Content(reference, tc.actual, nil, tolerances), tc.want)
		})
	}
}

// Chunks that are equal on both sides cancel out. The rest share a name, so their paths get an
// index.
func TestUnmatchedChunksOfTheSameName(t *testing.T) {
	const name = "traces[priority=1,service=web,name=http.request,resource=GET /,index="
	reference := traces(
		chunk(1, "trace-1", "span-1", "span-2", "span-1", "SELECT 1"),
		chunk(1, "trace-2", "span-3", "span-4", "span-3", "SELECT 2"),
		chunk(1, "trace-3", "span-5", "span-6", "span-5", "SELECT 3"),
	)
	actual := traces(
		chunk(1, "trace-1", "span-1", "span-2", "span-1", "SELECT 2"),
		chunk(1, "trace-2", "span-3", "span-4", "span-3", "SELECT 4"),
		chunk(1, "trace-3", "span-5", "span-6", "span-5", "SELECT 5"),
	)
	checkDifferences(t, Content(reference, actual, nil, tolerances), []diff.Difference{
		{Area: "traces", Path: name + "0].chunk.spans[1].resource", Reference: "SELECT 1", Actual: "SELECT 4"},
		{Area: "traces", Path: name + "1].chunk.spans[1].resource", Reference: "SELECT 3", Actual: "SELECT 5"},
	})
	got := paths(Content(reference, traces(chunk(1, "trace-1", "span-1", "span-2", "span-1", "SELECT 2")), nil, tolerances))
	want := []string{"traces " + name + "0]", "traces " + name + "1]"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %q\nwant %q", got, want)
	}
}
