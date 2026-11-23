package compare

import (
	"reflect"
	"testing"

	"github.com/DataDog/serverless-components/parity/internal/diff"
	"github.com/DataDog/serverless-components/parity/internal/normalize"
	"github.com/DataDog/serverless-components/parity/internal/scenario"
	"github.com/DataDog/serverless-components/parity/internal/timing"
)

var tolerances = timing.DefaultTolerances()

func series(metric, kind string, values ...float64) normalize.Series {
	s := normalize.Series{Metric: metric, Type: kind, Tags: []string{"env:test"}, Points: []normalize.Point{}}
	for _, v := range values {
		s.Points = append(s.Points, normalize.Point{Timestamp: normalize.Timestamp{Value: 1}, Value: v})
	}
	return s
}

func checkDifferences(t *testing.T, got, want []diff.Difference) {
	t.Helper()
	if len(got) == 0 && len(want) == 0 {
		return
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("differences:\n got %#v\nwant %#v", got, want)
	}
}

// paths returns the area and path of each difference.
func paths(diffs []diff.Difference) []string {
	var out []string
	for _, d := range diffs {
		out = append(out, d.Area+" "+d.Path)
	}
	return out
}

func TestLogsCompareInOrder(t *testing.T) {
	log := func(message string) normalize.Log {
		return normalize.Log{Message: message, Source: "parity", Service: "web", Status: "info"}
	}
	reference := normalize.Normalized{Logs: []normalize.Log{log("first"), log("second")}}
	actual := normalize.Normalized{Logs: []normalize.Log{log("second"), log("first")}}
	checkDifferences(t, Content(reference, actual, nil, tolerances), []diff.Difference{
		{Area: "logs", Path: "logs[ddsource=parity,index=0].message", Reference: "first", Actual: "second"},
		{Area: "logs", Path: "logs[ddsource=parity,index=1].message", Reference: "second", Actual: "first"},
	})
}

func TestProcessExitCodeDifference(t *testing.T) {
	reference := normalize.Normalized{Process: normalize.ProcessView{ExitCode: 3}}
	actual := normalize.Normalized{Process: normalize.ProcessView{ExitCode: 1}}
	checkDifferences(t, Content(reference, actual, nil, tolerances), []diff.Difference{
		{Area: "process", Path: "process.exit_code", Reference: 3, Actual: 1},
	})
}

func TestOverrideGreaterThan(t *testing.T) {
	const path = "series[metric=datadog.serverless.cpu_usage,type=gauge,tags=env:test].value"
	reference := normalize.Normalized{Series: []normalize.Series{series("datadog.serverless.cpu_usage", "gauge", 2)}}
	for _, tc := range []struct {
		name      string
		actual    float64
		overrides []scenario.Override
		want      []diff.Difference
	}{
		{"greater than passes", 5, []scenario.Override{{Path: path, Comparator: "greater_than", Value: 0}}, nil},
		{"greater than fails", 0, []scenario.Override{{Path: path, Comparator: "greater_than", Value: 0}},
			[]diff.Difference{{Area: "series", Path: path, Reference: 2.0, Actual: 0.0}}},
		{"wildcard path", 5, []scenario.Override{{Path: "series[metric=datadog.serverless.cpu_usage,*].value", Comparator: "greater_than", Value: 0}}, nil},
		{"no override is exact", 5, nil, []diff.Difference{{Area: "series", Path: path, Reference: 2.0, Actual: 5.0}}},
		{"explicit exact", 5, []scenario.Override{{Path: path, Comparator: "exact"}},
			[]diff.Difference{{Area: "series", Path: path, Reference: 2.0, Actual: 5.0}}},
		{"equals passes", 4, []scenario.Override{{Path: path, Comparator: "equals", Value: 4}}, nil},
		{"equals fails", 5, []scenario.Override{{Path: path, Comparator: "equals", Value: 4}},
			[]diff.Difference{{Area: "series", Path: path, Reference: 2.0, Actual: 5.0}}},
		{"within percent passes", 2.1, []scenario.Override{{Path: path, Comparator: "within_percent", Value: 10}}, nil},
		{"within percent fails", 2.5, []scenario.Override{{Path: path, Comparator: "within_percent", Value: 10}},
			[]diff.Difference{{Area: "series", Path: path, Reference: 2.0, Actual: 2.5}}},
		{"unknown comparator fails", 2, []scenario.Override{{Path: path, Comparator: "close_enough"}},
			[]diff.Difference{{Area: "series", Path: path, Reference: 2.0, Actual: 2.0}}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			actual := normalize.Normalized{Series: []normalize.Series{series("datadog.serverless.cpu_usage", "gauge", tc.actual)}}
			checkDifferences(t, Content(reference, actual, tc.overrides, tolerances), tc.want)
		})
	}
}

func TestOverrideDoesNotHideAMissingItem(t *testing.T) {
	reference := normalize.Normalized{Series: []normalize.Series{series("datadog.serverless.cpu_usage", "gauge", 2)}}
	overrides := []scenario.Override{{Path: "series[*].value", Comparator: "greater_than", Value: 0}}
	got := Content(reference, normalize.Normalized{}, overrides, tolerances)
	if len(got) != 1 || got[0].Actual != nil {
		t.Fatalf("got %#v, want one difference with no actual value", got)
	}
}

func TestMissingItemHasNilSide(t *testing.T) {
	reference := normalize.Normalized{Series: []normalize.Series{series("parity.requests", "count", 1)}}
	actual := normalize.Normalized{Series: []normalize.Series{series("parity.errors", "count", 1)}}
	checkDifferences(t, Content(reference, actual, nil, tolerances), []diff.Difference{
		{Area: "series", Path: "series[metric=parity.errors,type=count,tags=env:test]", Reference: nil,
			Actual: seriesGroup{Value: 1, Attributes: map[string][]string{"points.timestamp": {"1"}}}},
		{Area: "series", Path: "series[metric=parity.requests,type=count,tags=env:test]", Actual: nil,
			Reference: seriesGroup{Value: 1, Attributes: map[string][]string{"points.timestamp": {"1"}}}},
	})
}

func TestUndecodedIsAlwaysADifference(t *testing.T) {
	indexed := normalize.Undecoded{Route: "/api/v0.2/traces", Error: "indexed trace format is not compared yet"}
	unknown := normalize.Undecoded{Route: "/api/v1/unknown", Error: "no decoder"}
	reference := normalize.Normalized{Undecoded: []normalize.Undecoded{indexed}}
	actual := normalize.Normalized{Undecoded: []normalize.Undecoded{indexed, unknown}}
	checkDifferences(t, Content(reference, actual, nil, tolerances), []diff.Difference{
		{Area: "undecoded", Path: "undecoded[route=/api/v0.2/traces,error=indexed trace format is not compared yet]", Reference: indexed, Actual: indexed},
		{Area: "undecoded", Path: "undecoded[route=/api/v1/unknown,error=no decoder]", Reference: nil, Actual: unknown},
	})
}

func TestDifferencesAreSorted(t *testing.T) {
	logs := func(messages ...string) []normalize.Log {
		var out []normalize.Log
		for _, m := range messages {
			out = append(out, normalize.Log{Message: m, Source: "parity"})
		}
		return out
	}
	reference := normalize.Normalized{
		Logs:    logs("0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10"),
		Process: normalize.ProcessView{ExitCode: 1, Signals: []string{"SIGTERM"}},
		Series:  []normalize.Series{series("parity.b", "count", 1), series("parity.a", "count", 1)},
	}
	actual := normalize.Normalized{
		Logs:    logs("0", "1", "x", "3", "4", "5", "6", "7", "8", "9", "y"),
		Process: normalize.ProcessView{ExitCode: 0},
	}
	want := []string{
		"logs logs[ddsource=parity,index=2].message",
		"logs logs[ddsource=parity,index=10].message",
		"process process.exit_code",
		"process process.signals",
		"series series[metric=parity.a,type=count,tags=env:test]",
		"series series[metric=parity.b,type=count,tags=env:test]",
	}
	if got := paths(Content(reference, actual, nil, tolerances)); !reflect.DeepEqual(got, want) {
		t.Fatalf("got %q\nwant %q", got, want)
	}
}

func TestCompareWithoutReferenceIsNotAPass(t *testing.T) {
	reference := &normalize.Normalized{}
	failure := diff.Difference{Area: "process", Path: "process.exit_code"}
	expected := diff.Difference{Area: "traces", Path: "traces[priority=0]", Expected: true, Justification: "implementation.local-priority-0-drop"}
	undecoded := diff.Difference{Area: "undecoded", Path: "undecoded[route=/api/v1/unknown,error=no decoder]"}
	undecodedExpected := undecoded
	undecodedExpected.Expected, undecodedExpected.Justification = true, "implementation.local-priority-0-drop"
	for _, tc := range []struct {
		name      string
		reference *normalize.Normalized
		diffs     []diff.Difference
		want      string
	}{
		{"no reference and no differences", nil, nil, "no-reference"},
		{"no reference with differences", nil, []diff.Difference{failure}, "no-reference"},
		{"no differences", reference, nil, "pass"},
		{"only expected differences", reference, []diff.Difference{expected}, "expected-only"},
		{"an unexpected difference", reference, []diff.Difference{expected, failure}, "fail"},
		{"an undecoded payload", reference, []diff.Difference{undecoded}, "fail"},
		{"an undecoded payload marked expected", reference, []diff.Difference{expected, undecodedExpected}, "fail"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := Status(tc.reference, tc.diffs); got != tc.want {
				t.Fatalf("got %q, want %q", got, tc.want)
			}
		})
	}
}
