package compare

import (
	"math"
	"reflect"
	"testing"

	"github.com/DataDog/serverless-components/parity/internal/diff"
	"github.com/DataDog/serverless-components/parity/internal/normalize"
)

func TestSeriesBatchedDifferentlyStillMatch(t *testing.T) {
	reference := normalize.Normalized{Series: []normalize.Series{
		series("parity.requests", "count", 1), series("parity.requests", "count", 1), series("parity.requests", "count", 1),
	}}
	actual := normalize.Normalized{Series: []normalize.Series{series("parity.requests", "count", 3)}}
	checkDifferences(t, Content(reference, actual, nil, tolerances), nil)
}

func TestGaugeComparesLastValue(t *testing.T) {
	reference := normalize.Normalized{Series: []normalize.Series{
		series("parity.temperature", "gauge", 1), series("parity.temperature", "gauge", 2),
	}}
	for _, tc := range []struct {
		name   string
		actual []normalize.Series
		want   []diff.Difference
	}{
		{"same last value", []normalize.Series{series("parity.temperature", "gauge", 2)}, nil},
		{"same last value in one series", []normalize.Series{series("parity.temperature", "gauge", 5, 2)}, nil},
		{"different last value", []normalize.Series{
			series("parity.temperature", "gauge", 2), series("parity.temperature", "gauge", 1),
		}, []diff.Difference{{
			Area: "series", Path: "series[metric=parity.temperature,type=gauge,tags=env:test].value",
			Reference: 2.0, Actual: 1.0,
		}}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got := Content(reference, normalize.Normalized{Series: tc.actual}, nil, tolerances)
			checkDifferences(t, got, tc.want)
		})
	}
}

// Fields outside the points are part of the series key, so series that differ in them are
// never merged.
func TestSeriesKeyIncludesFields(t *testing.T) {
	withResource := func(name string, value float64) normalize.Series {
		s := series("parity.requests", "count", value)
		s.Resources = []normalize.Resource{{Type: "function", Name: name}}
		return s
	}
	withHost := series("parity.requests", "count", 1)
	withHost.Host = "<masked>"
	for _, tc := range []struct {
		name              string
		reference, actual []normalize.Series
		want              []string
	}{
		{"host on one side", []normalize.Series{withHost}, []normalize.Series{series("parity.requests", "count", 1)}, []string{
			"series series[metric=parity.requests,type=count,tags=env:test,host=<masked>]",
			"series series[metric=parity.requests,type=count,tags=env:test]",
		}},
		{"values swapped between resources",
			[]normalize.Series{withResource("a", 1), withResource("b", 2)},
			[]normalize.Series{withResource("a", 2), withResource("b", 1)},
			[]string{
				`series series[metric=parity.requests,type=count,tags=env:test,resources=[{"type":"function","name":"a"}]].value`,
				`series series[metric=parity.requests,type=count,tags=env:test,resources=[{"type":"function","name":"b"}]].value`,
			}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got := paths(Content(normalize.Normalized{Series: tc.reference}, normalize.Normalized{Series: tc.actual}, nil, tolerances))
			if !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("got %q\nwant %q", got, tc.want)
			}
		})
	}
}

func TestSketchPercentileWithinTolerance(t *testing.T) {
	const path = "sketches[metric=parity.latency,tags=env:test]"
	group := func(change func(*sketchGroup)) sketchGroup {
		g := sketchGroup{Count: 3, Sum: 300, Min: 50, Max: 150, Quantiles: map[string]float64{"p50": 100, "p95": 100}}
		if change != nil {
			change(&g)
		}
		return g
	}
	for _, tc := range []struct {
		name   string
		change func(*sketchGroup)
		want   []diff.Difference
	}{
		{"p95 within tolerance", func(g *sketchGroup) { g.Quantiles["p95"] = 100.5 }, nil},
		{"p95 outside tolerance", func(g *sketchGroup) { g.Quantiles["p95"] = 102 },
			[]diff.Difference{{Area: "sketches", Path: path + ".quantiles[p95]", Reference: 100.0, Actual: 102.0}}},
		{"p95 not a number", func(g *sketchGroup) { g.Quantiles["p95"] = math.NaN() }, nil},
		{"count is exact", func(g *sketchGroup) { g.Count = 4 },
			[]diff.Difference{{Area: "sketches", Path: path + ".count", Reference: int64(3), Actual: int64(4)}}},
		{"sum is exact", func(g *sketchGroup) { g.Sum = 300.5 },
			[]diff.Difference{{Area: "sketches", Path: path + ".sum", Reference: 300.0, Actual: 300.5}}},
		{"minimum is exact", func(g *sketchGroup) { g.Min = 50.1 },
			[]diff.Difference{{Area: "sketches", Path: path + ".min", Reference: 50.0, Actual: 50.1}}},
		{"maximum is exact", func(g *sketchGroup) { g.Max = 150.1 },
			[]diff.Difference{{Area: "sketches", Path: path + ".max", Reference: 150.0, Actual: 150.1}}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			c := comparer{area: "sketches", tolerances: tolerances}
			c.compare(path, reflect.ValueOf(group(nil)), reflect.ValueOf(group(tc.change)))
			if tc.name == "p95 not a number" {
				// NaN never equals itself, so the expected value can't be written literally.
				if len(c.differences) != 1 || c.differences[0].Path != path+".quantiles[p95]" {
					t.Fatalf("got %#v, want one difference at the p95", c.differences)
				}
				return
			}
			checkDifferences(t, c.differences, tc.want)
		})
	}
}

// The sketch keys are those of the agent's default sketch configuration: key 1635 holds 100
// and key 1680 holds 200. Key 1680's lower bound is 1.015625^342, about 200.8246.
func TestSketchesMergeBins(t *testing.T) {
	sketch := func(points ...normalize.SketchPoint) normalize.Sketch {
		return normalize.Sketch{Metric: "parity.latency", Tags: []string{"env:test"}, Points: points}
	}
	one := normalize.SketchPoint{Count: 1, Min: 100, Max: 100, Avg: 100, Sum: 100, K: []int32{1635}, N: []uint32{1}}
	two := normalize.SketchPoint{Count: 1, Min: 200, Max: 200, Avg: 200, Sum: 200, K: []int32{1680}, N: []uint32{1}}
	both := normalize.SketchPoint{Count: 2, Min: 100, Max: 200, Avg: 150, Sum: 300, K: []int32{1635, 1680}, N: []uint32{1, 1}}

	got := sketchGroups([]normalize.Sketch{sketch(one), sketch(two)})
	want := map[string]any{"sketches[metric=parity.latency,tags=env:test]": sketchGroup{
		Count: 2, Sum: 300, Min: 100, Max: 200,
		Quantiles: map[string]float64{"p50": 100, "p75": 200.824580926909, "p90": 200.824580926909, "p95": 200.824580926909, "p99": 200.824580926909},
	}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %#v\nwant %#v", got, want)
	}
	reference := normalize.Normalized{Sketches: []normalize.Sketch{sketch(one, two)}}
	actual := normalize.Normalized{Sketches: []normalize.Sketch{sketch(both)}}
	checkDifferences(t, Content(reference, actual, nil, tolerances), nil)
}

// The agent keeps key 32767 for values too large for the sketch. The quantile in that bin is
// infinite, never NaN.
func TestSketchQuantileInfiniteBin(t *testing.T) {
	got := sketchQuantile(map[int32]uint64{1635: 1, 32767: 1}, 2, 100, math.Inf(1), 0.99)
	if !math.IsInf(got, 1) {
		t.Fatalf("got %v, want +Inf", got)
	}
}

// Check runs are grouped by check, status and sorted tags. The agent sends one per flush, so how
// many arrive is not compared.
func TestCheckRunsCompareByCheckStatusAndTags(t *testing.T) {
	run := func(status int, tags ...string) normalize.CheckRun {
		return normalize.CheckRun{Check: "datadog.agent.up", HostName: "<masked>", Timestamp: normalize.Timestamp{Value: 1}, Status: status, Tags: tags}
	}
	reference := []normalize.CheckRun{run(0, "b:1", "a:1"), run(0, "a:1", "b:1")}
	for _, tc := range []struct {
		name   string
		actual []normalize.CheckRun
		want   []string
	}{
		{"fewer runs, tags in another order", []normalize.CheckRun{run(0, "b:1", "a:1")}, nil},
		{"another status", []normalize.CheckRun{run(2, "a:1", "b:1")}, []string{
			"check_runs check_runs[check=datadog.agent.up,status=0,tags=a:1|b:1,host_name=<masked>]",
			"check_runs check_runs[check=datadog.agent.up,status=2,tags=a:1|b:1,host_name=<masked>]",
		}},
		{"another message", []normalize.CheckRun{func() normalize.CheckRun { r := run(0, "a:1", "b:1"); r.Message = "down"; return r }()}, []string{
			"check_runs check_runs[check=datadog.agent.up,status=0,tags=a:1|b:1,host_name=<masked>,message=down]",
			"check_runs check_runs[check=datadog.agent.up,status=0,tags=a:1|b:1,host_name=<masked>]",
		}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got := paths(Content(normalize.Normalized{CheckRuns: reference}, normalize.Normalized{CheckRuns: tc.actual}, nil, tolerances))
			if !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("got %q\nwant %q", got, tc.want)
			}
		})
	}
}
