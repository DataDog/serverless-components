package compare

import (
	"reflect"
	"testing"

	"github.com/DataDog/serverless-components/parity/internal/diff"
	"github.com/DataDog/serverless-components/parity/internal/normalize"
)

// gamma is the index mapping of a DDSketch with a relative accuracy of 0.01. In it, bin 195
// holds about 50, bin 230 about 100 (its value is 100.49456770856489) and bin 250 about 150.
const gamma = 1.02020202020202

func latency(bins map[int32]float64) *normalize.Summary {
	return &normalize.Summary{Gamma: gamma, Positive: bins}
}

func stats(buckets ...normalize.StatsBucket) normalize.Normalized {
	return normalize.Normalized{Stats: []normalize.StatsPayload{{
		AgentEnv: "test", Stats: []normalize.ClientStats{{Env: "test", Buckets: buckets}},
	}}}
}

func statsBucket(status uint32, hits uint64, summary *normalize.Summary) normalize.StatsBucket {
	return normalize.StatsBucket{Duration: 10, Stats: []normalize.GroupedStats{{
		Service: "web", Name: "http.request", Resource: "GET /", HTTPStatusCode: status,
		Hits: hits, Duration: hits * 100, OkSummary: summary,
	}}}
}

func TestStatsMergeBucketsAndSummaries(t *testing.T) {
	const group = "stats[service=web,name=http.request,resource=GET /,http_status_code=200,client.env=test,payload.agent_env=test]"
	hundred := map[string]float64{"p50": 100.49456770856489, "p75": 100.49456770856489, "p90": 100.49456770856489, "p95": 100.49456770856489, "p99": 100.49456770856489}
	twoBuckets := stats(statsBucket(200, 1, latency(map[int32]float64{230: 1})), statsBucket(200, 1, latency(map[int32]float64{230: 1})))
	fiftyAndOneFifty := stats(statsBucket(200, 1, latency(map[int32]float64{195: 1})), statsBucket(200, 1, latency(map[int32]float64{250: 1})))
	otherMapping := &normalize.Summary{Gamma: 1.04, Positive: map[int32]float64{117: 1}}
	mixedMappings := stats(statsBucket(200, 1, latency(map[int32]float64{230: 1})), statsBucket(200, 1, otherMapping))
	for _, tc := range []struct {
		name              string
		reference, actual normalize.Normalized
		want              []diff.Difference
	}{
		{"merged into one bucket", twoBuckets, stats(statsBucket(200, 2, latency(map[int32]float64{230: 2}))), nil},
		{"split across buckets differently", fiftyAndOneFifty, stats(statsBucket(200, 2, latency(map[int32]float64{195: 1, 250: 1}))), nil},
		{"count is exact", twoBuckets, stats(statsBucket(200, 2, latency(map[int32]float64{230: 3}))),
			[]diff.Difference{{Area: "stats", Path: group + ".ok_summary.count", Reference: 2.0, Actual: 3.0}}},
		{"hits are exact", twoBuckets, stats(statsBucket(200, 1, latency(map[int32]float64{230: 2}))), []diff.Difference{
			{Area: "stats", Path: group + ".duration", Reference: uint64(200), Actual: uint64(100)},
			{Area: "stats", Path: group + ".hits", Reference: uint64(2), Actual: uint64(1)},
		}},
		{"summary missing", twoBuckets, stats(statsBucket(200, 2, nil)), []diff.Difference{
			{Area: "stats", Path: group + ".ok_summary", Reference: summaryGroup{Count: 2, Quantiles: hundred}, Actual: nil},
		}},
		{"different index mappings are never a pass", mixedMappings, mixedMappings, []diff.Difference{{
			Area: "stats", Path: group + ".ok_summary.unmerged",
			Reference: "the summaries use different index mappings: gamma=1.02020202020202,index_offset=0,interpolation=; gamma=1.04,index_offset=0,interpolation=",
			Actual:    "the summaries use different index mappings: gamma=1.02020202020202,index_offset=0,interpolation=; gamma=1.04,index_offset=0,interpolation=",
		}}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			checkDifferences(t, Content(tc.reference, tc.actual, nil, tolerances), tc.want)
		})
	}
}

// Latencies of 50 and 150 in two buckets are not latencies of 100: the merged sketch differs at
// every quantile.
func TestStatsSummaryQuantilesComeFromTheMergedSketch(t *testing.T) {
	const group = "stats[service=web,name=http.request,resource=GET /,http_status_code=200,client.env=test,payload.agent_env=test]"
	reference := stats(statsBucket(200, 1, latency(map[int32]float64{195: 1})), statsBucket(200, 1, latency(map[int32]float64{250: 1})))
	actual := stats(statsBucket(200, 2, latency(map[int32]float64{230: 2})))
	want := []string{
		"stats " + group + ".ok_summary.quantiles[p50]", "stats " + group + ".ok_summary.quantiles[p75]",
		"stats " + group + ".ok_summary.quantiles[p90]", "stats " + group + ".ok_summary.quantiles[p95]",
		"stats " + group + ".ok_summary.quantiles[p99]",
	}
	if got := paths(Content(reference, actual, nil, tolerances)); !reflect.DeepEqual(got, want) {
		t.Fatalf("got %q\nwant %q", got, want)
	}
}

func TestStatsSummaryQuantileTolerance(t *testing.T) {
	const path = "stats[service=web].ok_summary.quantiles[p95]"
	group := func(p95 float64) statsGroup {
		return statsGroup{Hits: 1, OkSummary: &summaryGroup{Count: 1, Quantiles: map[string]float64{"p95": p95}}}
	}
	for _, tc := range []struct {
		name   string
		actual float64
		want   []diff.Difference
	}{
		{"within tolerance", 100.5, nil},
		{"outside tolerance", 102, []diff.Difference{{Area: "stats", Path: path, Reference: 100.0, Actual: 102.0}}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			c := comparer{area: "stats", tolerances: tolerances}
			c.compare("stats[service=web]", reflect.ValueOf(group(100)), reflect.ValueOf(group(tc.actual)))
			checkDifferences(t, c.differences, tc.want)
		})
	}
}

func TestStatsKeyFieldsSeparateGroups(t *testing.T) {
	reference := stats(statsBucket(200, 1, nil))
	actual := stats(statsBucket(500, 1, nil))
	want := []string{
		"stats stats[service=web,name=http.request,resource=GET /,http_status_code=200,client.env=test,payload.agent_env=test]",
		"stats stats[service=web,name=http.request,resource=GET /,http_status_code=500,client.env=test,payload.agent_env=test]",
	}
	if got := paths(Content(reference, actual, nil, tolerances)); !reflect.DeepEqual(got, want) {
		t.Fatalf("got %q\nwant %q", got, want)
	}
}
