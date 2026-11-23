package compare

import (
	"fmt"
	"slices"
	"strings"

	"github.com/DataDog/serverless-components/parity/internal/diff"
	"github.com/DataDog/serverless-components/parity/internal/normalize"
	"github.com/DataDog/sketches-go/ddsketch"
	"github.com/DataDog/sketches-go/ddsketch/pb/sketchpb"
)

// statsGroup is the stats of one group, merged over buckets and payloads.
type statsGroup struct {
	Hits         uint64        `json:"hits"`
	Errors       uint64        `json:"errors"`
	TopLevelHits uint64        `json:"top_level_hits"`
	Duration     uint64        `json:"duration"`
	OkSummary    *summaryGroup `json:"ok_summary"`
	ErrorSummary *summaryGroup `json:"error_summary"`
	// Attributes hold the bucket fields and the tracer's sequence number.
	Attributes map[string][]string `json:"attributes,omitempty"`
}

// summaryGroup is latency summaries merged bin by bin. Count and ZeroCount are compared exactly,
// and the quantiles, computed from the merged sketch, within the percentile tolerance. Unmerged
// says why there are no quantiles although the count is not 0: the summaries use different
// index mappings, or the merged sketch is invalid. Content always reports it as a difference.
type summaryGroup struct {
	Count     float64            `json:"count"`
	ZeroCount float64            `json:"zero_count"`
	Quantiles map[string]float64 `json:"quantiles,omitempty"`
	Unmerged  string             `json:"unmerged,omitempty"`
}

var statsCounters = []string{"hits", "errors", "top_level_hits", "duration", "ok_summary", "error_summary"}

func statsGroups(payloads []normalize.StatsPayload) map[string]any {
	type member struct {
		stats  normalize.GroupedStats
		bucket normalize.StatsBucket
		client normalize.ClientStats
	}
	members := map[string][]member{}
	for _, p := range payloads {
		payloadKey := keyFields("payload.", p, nil, "stats")
		for _, client := range p.Stats {
			clientKey := keyFields("client.", client, nil, "buckets", "sequence")
			for _, b := range client.Buckets {
				for _, s := range b.Stats {
					key := "stats[" + strings.Join(slices.Concat(
						keyFields("", s, []string{"service", "name", "resource"}, statsCounters...),
						clientKey, payloadKey), ",") + "]"
					members[key] = append(members[key], member{stats: s, bucket: b, client: client})
				}
			}
		}
	}
	groups := map[string]any{}
	for key, list := range members {
		var g statsGroup
		var buckets, clients []any
		var ok, failed []*normalize.Summary
		for _, m := range list {
			g.Hits += m.stats.Hits
			g.Errors += m.stats.Errors
			g.TopLevelHits += m.stats.TopLevelHits
			g.Duration += m.stats.Duration
			ok = append(ok, m.stats.OkSummary)
			failed = append(failed, m.stats.ErrorSummary)
			buckets = append(buckets, m.bucket)
			clients = append(clients, struct {
				Sequence uint64 `json:"sequence,omitempty"`
			}{m.client.Sequence})
		}
		g.OkSummary, g.ErrorSummary = mergeSummaries(ok), mergeSummaries(failed)
		g.Attributes = map[string][]string{}
		attributes(g.Attributes, "bucket.", buckets, "stats")
		attributes(g.Attributes, "client.", clients)
		g.Attributes = nilIfEmpty(g.Attributes)
		groups[key] = g
	}
	return groups
}

// mergeSummaries merges the summaries of a group, ignoring the buckets that have none. It
// returns nil when no bucket has one.
func mergeSummaries(summaries []*normalize.Summary) *summaryGroup {
	var merged *normalize.Summary
	var mappings []string
	for _, s := range summaries {
		if s == nil {
			continue
		}
		mapping := fmt.Sprintf("gamma=%v,index_offset=%v,interpolation=%s", s.Gamma, s.IndexOffset, s.Interpolation)
		if !slices.Contains(mappings, mapping) {
			mappings = append(mappings, mapping)
		}
		if merged == nil {
			merged = &normalize.Summary{Gamma: s.Gamma, IndexOffset: s.IndexOffset, Interpolation: s.Interpolation,
				Positive: map[int32]float64{}, Negative: map[int32]float64{}}
		}
		merged.ZeroCount += s.ZeroCount
		for index, count := range s.Positive {
			merged.Positive[index] += count
		}
		for index, count := range s.Negative {
			merged.Negative[index] += count
		}
	}
	if merged == nil {
		return nil
	}
	out := &summaryGroup{ZeroCount: merged.ZeroCount, Count: merged.ZeroCount}
	for _, store := range []map[int32]float64{merged.Positive, merged.Negative} {
		for _, count := range store {
			out.Count += count
		}
	}
	if out.Count == 0 {
		return out
	}
	if len(mappings) > 1 {
		slices.Sort(mappings)
		out.Unmerged = "the summaries use different index mappings: " + strings.Join(mappings, "; ")
		return out
	}
	quantiles, err := summaryQuantiles(merged)
	if err != nil {
		out.Unmerged = err.Error()
		return out
	}
	out.Quantiles = quantiles
	return out
}

// summaryQuantiles computes the quantiles of a summary with sketches-go, as the Go agent does.
func summaryQuantiles(s *normalize.Summary) (map[string]float64, error) {
	interpolation := sketchpb.IndexMapping_NONE
	if s.Interpolation != "" {
		value, ok := sketchpb.IndexMapping_Interpolation_value[s.Interpolation]
		if !ok {
			return nil, fmt.Errorf("unknown interpolation %q", s.Interpolation)
		}
		interpolation = sketchpb.IndexMapping_Interpolation(value)
	}
	sketch, err := ddsketch.FromProto(&sketchpb.DDSketch{
		Mapping:        &sketchpb.IndexMapping{Gamma: s.Gamma, IndexOffset: s.IndexOffset, Interpolation: interpolation},
		PositiveValues: &sketchpb.Store{BinCounts: s.Positive},
		NegativeValues: &sketchpb.Store{BinCounts: s.Negative},
		ZeroCount:      s.ZeroCount,
	})
	if err != nil {
		return nil, err
	}
	out := map[string]float64{}
	for _, q := range quantiles {
		if out[q.name], err = sketch.GetValueAtQuantile(q.quantile); err != nil {
			return nil, err
		}
	}
	return out, nil
}

// unmergedSummaries reports the summaries that could not be merged and that the comparison
// reported nothing for, because they are the same on both sides. A summary that was not
// compared is never a pass.
func unmergedSummaries(reference, actual map[string]any, reported []diff.Difference) []diff.Difference {
	var out []diff.Difference
	for key, r := range reference {
		a, ok := actual[key]
		if !ok {
			continue
		}
		rg, ag := r.(statsGroup), a.(statsGroup)
		for _, s := range []struct {
			name              string
			reference, actual *summaryGroup
		}{{"ok_summary", rg.OkSummary, ag.OkSummary}, {"error_summary", rg.ErrorSummary, ag.ErrorSummary}} {
			if s.reference == nil || s.actual == nil || (s.reference.Unmerged == "" && s.actual.Unmerged == "") {
				continue
			}
			path := key + "." + s.name + ".unmerged"
			if !slices.ContainsFunc(reported, func(d diff.Difference) bool { return d.Path == path }) {
				out = append(out, diff.Difference{Area: "stats", Path: path, Reference: s.reference.Unmerged, Actual: s.actual.Unmerged})
			}
		}
	}
	return out
}
