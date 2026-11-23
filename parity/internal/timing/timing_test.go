package timing

import (
	"encoding/json"
	"reflect"
	"testing"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/diff"
)

var start = time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)

// at returns the time a number of milliseconds after start.
func at(milliseconds int) time.Time {
	return start.Add(time.Duration(milliseconds) * time.Millisecond)
}

func measured(d time.Duration) Measurement {
	return Measurement{Measurable: true, Value: Duration(d)}
}

var notMeasurable = Measurement{}

// allAreas returns a measure map where every area is not measurable, with the given overrides.
func allAreas(overrides map[string]Measurement) map[string]Measurement {
	out := map[string]Measurement{
		"series": notMeasurable, "sketches": notMeasurable, "logs": notMeasurable,
		"traces": notMeasurable, "stats": notMeasurable, "check_runs": notMeasurable,
	}
	for area, m := range overrides {
		out[area] = m
	}
	return out
}

// flushes returns, for each start in milliseconds, a burst of payloads of the area that
// arrive 10 milliseconds apart, each carrying the items.
func flushes(area string, burst int, items []string, starts ...int) []Arrival {
	var out []Arrival
	for _, s := range starts {
		for i := 0; i < burst; i++ {
			out = append(out, Arrival{Area: area, At: at(s + 10*i), Items: items})
		}
	}
	return out
}

func TestMeasure(t *testing.T) {
	tests := []struct {
		name string
		in   Input
		want Measures
	}{
		{
			name: "median interval inside the window, delay per area, missing item, shutdown",
			in: Input{
				Sent: []Sent{
					{At: at(1000), Items: []string{"metric:a", "span:r"}},
					{At: at(9000), Items: []string{"metric:lost"}},
					{At: at(11000), Items: []string{"log:late"}},
				},
				Arrivals: []Arrival{
					// Before the first send: outside the interval window.
					{Area: "series", At: at(500), Items: []string{"metric:a"}},
					{Area: "series", At: at(3000), Items: []string{"metric:a"}},
					{Area: "series", At: at(6000), Items: []string{"metric:a"}},
					// The agent's own metric: not sent by the workload, so outside the interval.
					{Area: "series", At: at(7000), Items: []string{"metric:agent"}},
					{Area: "series", At: at(10000), Items: []string{"metric:a"}},
					{Area: "traces", At: at(1500), Items: []string{"span:r"}},
					{Area: "stats", At: at(4000), Items: []string{"span:r"}},
					// After SIGTERM: outside the interval window, still counts for delay.
					{Area: "logs", At: at(11500), Items: []string{"log:late"}},
				},
				Sigterm: at(10500),
				Exited:  at(12500),
			},
			want: Measures{
				// Series flushes inside the window are 3s and 4s apart, so the median is 3.5s.
				IntervalByArea: allAreas(map[string]Measurement{"series": measured(3500 * time.Millisecond)}),
				// Every series arrival in the window, the agent's own at 7s included: 3s, 1s and 3s apart.
				FlushIntervalByArea: map[string]Measurement{"series": measured(3 * time.Second)},
				MaxDelayByArea: allAreas(map[string]Measurement{
					"series": measured(2 * time.Second),
					"traces": measured(500 * time.Millisecond),
					"stats":  measured(3 * time.Second),
					"logs":   measured(500 * time.Millisecond),
				}),
				MissingAfterSigterm:     []string{"metric:lost"},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{},
				Sigterm:                 true,
				Shutdown:                measured(2 * time.Second),
			},
		},
		{
			name: "odd count median, no SIGTERM uses exit as the window end and skips shutdown",
			in: Input{
				Sent:     []Sent{{At: at(0), Items: []string{"metric:a"}}},
				Arrivals: flushes("series", 1, []string{"metric:a"}, 3000, 6000, 9000, 19000),
				Exited:   at(20000),
			},
			want: Measures{
				IntervalByArea:          allAreas(map[string]Measurement{"series": measured(3 * time.Second)}),
				FlushIntervalByArea:     map[string]Measurement{"series": measured(3 * time.Second)},
				MaxDelayByArea:          allAreas(map[string]Measurement{"series": measured(3 * time.Second)}),
				MissingAfterSigterm:     []string{},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{},
				Shutdown:                notMeasurable,
			},
		},
		{
			name: "bursts of two payloads per flush give the flush interval",
			in: Input{
				Sent:     []Sent{{At: at(0), Items: []string{"metric:a"}}},
				Arrivals: flushes("series", 2, []string{"metric:a"}, 3000, 6000, 9000, 12000),
				Exited:   at(13000),
			},
			want: Measures{
				IntervalByArea:          allAreas(map[string]Measurement{"series": measured(3 * time.Second)}),
				FlushIntervalByArea:     map[string]Measurement{"series": measured(3 * time.Second)},
				MaxDelayByArea:          allAreas(map[string]Measurement{"series": measured(3 * time.Second)}),
				MissingAfterSigterm:     []string{},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{},
				Shutdown:                notMeasurable,
			},
		},
		{
			name: "bursts of three payloads per flush give the same flush interval",
			in: Input{
				Sent:     []Sent{{At: at(0), Items: []string{"metric:a"}}},
				Arrivals: flushes("series", 3, []string{"metric:a"}, 3000, 6000, 9000, 12000),
				Exited:   at(13000),
			},
			want: Measures{
				IntervalByArea:          allAreas(map[string]Measurement{"series": measured(3 * time.Second)}),
				FlushIntervalByArea:     map[string]Measurement{"series": measured(3 * time.Second)},
				MaxDelayByArea:          allAreas(map[string]Measurement{"series": measured(3 * time.Second)}),
				MissingAfterSigterm:     []string{},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{},
				Shutdown:                notMeasurable,
			},
		},
		{
			name: "two flushes are not measurable",
			in: Input{
				Sent:     []Sent{{At: at(0), Items: []string{"metric:a"}}},
				Arrivals: flushes("series", 1, []string{"metric:a"}, 3000, 6000),
				Exited:   at(7000),
			},
			want: Measures{
				IntervalByArea:          allAreas(nil),
				MaxDelayByArea:          allAreas(map[string]Measurement{"series": measured(3 * time.Second)}),
				MissingAfterSigterm:     []string{},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{},
				Shutdown:                notMeasurable,
			},
		},
		{
			name: "check runs, which the workload never sends, have only a flush interval",
			in: Input{
				Sent:     []Sent{{At: at(0), Items: []string{"metric:a"}}},
				Arrivals: flushes("check_runs", 1, []string{"check:datadog.agent.up"}, 3000, 6000, 9000),
				Exited:   at(10000),
			},
			want: Measures{
				IntervalByArea:          allAreas(nil),
				FlushIntervalByArea:     map[string]Measurement{"check_runs": measured(3 * time.Second)},
				MaxDelayByArea:          allAreas(nil),
				MissingAfterSigterm:     []string{"metric:a"},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{},
				Shutdown:                notMeasurable,
			},
		},
		{
			name: "one flush of several payloads in the window is not measurable",
			in: Input{
				Sent:     []Sent{{At: at(0), Items: []string{"metric:a"}}},
				Arrivals: flushes("series", 3, []string{"metric:a"}, 3000),
				Exited:   at(4000),
			},
			want: Measures{
				IntervalByArea:          allAreas(nil),
				MaxDelayByArea:          allAreas(map[string]Measurement{"series": measured(3 * time.Second)}),
				MissingAfterSigterm:     []string{},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{},
				Shutdown:                notMeasurable,
			},
		},
		{
			name: "one arrival in the window is not measurable, and an item sent after SIGTERM that never arrives is recorded",
			in: Input{
				Sent: []Sent{
					{At: at(0), Items: []string{"metric:a"}},
					{At: at(5000), Items: []string{"log:after", "log:after2"}},
				},
				Arrivals: []Arrival{
					{Area: "series", At: at(3000), Items: []string{"metric:a"}},
					{Area: "series", At: at(4500), Items: []string{"metric:a"}},
				},
				Sigterm: at(4000),
				Exited:  at(6000),
			},
			want: Measures{
				IntervalByArea:          allAreas(nil),
				MaxDelayByArea:          allAreas(map[string]Measurement{"series": measured(3 * time.Second)}),
				MissingAfterSigterm:     []string{},
				MissingSentAfterSigterm: []string{"log:after", "log:after2"},
				AmbiguousItems:          []string{},
				Sigterm:                 true,
				Shutdown:                measured(2 * time.Second),
			},
		},
		{
			name: "an arrival before the send does not deliver the item",
			in: Input{
				Sent:     []Sent{{At: at(2000), Items: []string{"metric:a"}}},
				Arrivals: []Arrival{{Area: "series", At: at(1000), Items: []string{"metric:a"}}},
				Sigterm:  at(3000),
				Exited:   at(4000),
			},
			want: Measures{
				IntervalByArea:          allAreas(nil),
				MaxDelayByArea:          allAreas(nil),
				MissingAfterSigterm:     []string{"metric:a"},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{},
				Sigterm:                 true,
				Shutdown:                measured(time.Second),
			},
		},
		{
			name: "an arrival after exit does not deliver the item",
			in: Input{
				Sent:     []Sent{{At: at(1000), Items: []string{"metric:a"}}},
				Arrivals: []Arrival{{Area: "series", At: at(4500), Items: []string{"metric:a"}}},
				Sigterm:  at(3000),
				Exited:   at(4000),
			},
			want: Measures{
				IntervalByArea:          allAreas(nil),
				MaxDelayByArea:          allAreas(nil),
				MissingAfterSigterm:     []string{"metric:a"},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{},
				Sigterm:                 true,
				Shutdown:                measured(time.Second),
			},
		},
		{
			name: "no exit after SIGTERM makes shutdown not measurable",
			in:   Input{Sigterm: at(1000)},
			want: Measures{
				IntervalByArea:          allAreas(nil),
				MaxDelayByArea:          allAreas(nil),
				MissingAfterSigterm:     []string{},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{},
				Sigterm:                 true,
				Shutdown:                notMeasurable,
			},
		},
		{
			name: "a metric matches an arrival whose tags include the sent tags",
			in: Input{
				Sent:     []Sent{{At: at(1000), Items: []string{"metric:m|route:/"}}},
				Arrivals: []Arrival{{Area: "series", At: at(3000), Items: []string{"metric:m|host:a,route:/"}}},
				Exited:   at(4000),
			},
			want: Measures{
				IntervalByArea:          allAreas(nil),
				MaxDelayByArea:          allAreas(map[string]Measurement{"series": measured(2 * time.Second)}),
				MissingAfterSigterm:     []string{},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{},
				Shutdown:                notMeasurable,
			},
		},
		{
			name: "a dropped final flush with distinct tags is missing",
			in: Input{
				Sent: []Sent{
					{At: at(7000), Items: []string{"metric:m|run:1"}},
					{At: at(8000), Items: []string{"metric:m|run:2"}},
				},
				// The arrival after the second send holds only the first send's series.
				Arrivals: []Arrival{{Area: "series", At: at(8500), Items: []string{"metric:m|host:a,run:1"}}},
				Sigterm:  at(9000),
				Exited:   at(10000),
			},
			want: Measures{
				IntervalByArea:          allAreas(nil),
				MaxDelayByArea:          allAreas(map[string]Measurement{"series": measured(1500 * time.Millisecond)}),
				MissingAfterSigterm:     []string{"metric:m|run:2"},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{},
				Sigterm:                 true,
				Shutdown:                measured(time.Second),
			},
		},
		{
			name: "a dropped final flush with identical keys is ambiguous and not used",
			in: Input{
				Sent: []Sent{
					{At: at(7000), Items: []string{"metric:m", "span:r"}},
					{At: at(8000), Items: []string{"metric:m"}},
				},
				Arrivals: []Arrival{
					{Area: "series", At: at(8500), Items: []string{"metric:m|host:a"}},
					{Area: "traces", At: at(7500), Items: []string{"span:r"}},
				},
				Sigterm: at(9000),
				Exited:  at(10000),
			},
			want: Measures{
				IntervalByArea:          allAreas(nil),
				MaxDelayByArea:          allAreas(map[string]Measurement{"traces": measured(500 * time.Millisecond)}),
				MissingAfterSigterm:     []string{},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{"metric:m"},
				Sigterm:                 true,
				Shutdown:                measured(time.Second),
			},
		},
		{
			name: "an arrived metric that matches sends of two batches makes both ambiguous",
			in: Input{
				Sent: []Sent{
					{At: at(7000), Items: []string{"metric:m|route:/"}},
					{At: at(8000), Items: []string{"metric:m|env:x"}},
				},
				Arrivals: []Arrival{{Area: "series", At: at(8500), Items: []string{"metric:m|env:x,route:/"}}},
				Sigterm:  at(9000),
				Exited:   at(10000),
			},
			want: Measures{
				IntervalByArea:          allAreas(nil),
				MaxDelayByArea:          allAreas(nil),
				MissingAfterSigterm:     []string{},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{"metric:m|env:x", "metric:m|route:/"},
				Sigterm:                 true,
				Shutdown:                measured(time.Second),
			},
		},
		{
			name: "sent metrics whose tags are a subset of each other are ambiguous, also in one batch",
			in: Input{
				Sent: []Sent{
					{At: at(1000), Items: []string{"metric:m", "metric:m|route:/", "metric:n|a:1"}},
					{At: at(2000), Items: []string{"log:x", "metric:n|a:2"}},
					{At: at(3000), Items: []string{"log:x"}},
				},
				Exited: at(4000),
			},
			want: Measures{
				IntervalByArea:          allAreas(nil),
				MaxDelayByArea:          allAreas(nil),
				MissingAfterSigterm:     []string{"metric:n|a:1", "metric:n|a:2"},
				MissingSentAfterSigterm: []string{},
				AmbiguousItems:          []string{"log:x", "metric:m", "metric:m|route:/"},
				Shutdown:                notMeasurable,
			},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			got := Measure(test.in, DefaultTolerances())
			if !reflect.DeepEqual(got, test.want) {
				t.Fatalf("measures:\n got %#v\nwant %#v", got, test.want)
			}
		})
	}
}

func TestMeasuresJSON(t *testing.T) {
	m := Measures{
		IntervalByArea:          map[string]Measurement{"series": measured(3500 * time.Millisecond), "logs": notMeasurable},
		MaxDelayByArea:          map[string]Measurement{"series": measured(2 * time.Second)},
		MissingAfterSigterm:     []string{"metric:lost"},
		MissingSentAfterSigterm: []string{},
		AmbiguousItems:          []string{"log:x"},
		Sigterm:                 true,
		Shutdown:                notMeasurable,
	}
	encoded, err := json.Marshal(m)
	if err != nil {
		t.Fatal(err)
	}
	want := `{"interval_by_area":{"logs":{"measurable":false},"series":{"measurable":true,"value":"3.5s"}},` +
		`"max_delay_by_area":{"series":{"measurable":true,"value":"2s"}},` +
		`"missing_after_sigterm":["metric:lost"],"missing_sent_after_sigterm":[],"ambiguous_items":["log:x"],` +
		`"sigterm":true,"shutdown":{"measurable":false}}`
	if string(encoded) != want {
		t.Fatalf("json:\n got %s\nwant %s", encoded, want)
	}
	var decoded Measures
	if err := json.Unmarshal(encoded, &decoded); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(decoded, m) {
		t.Fatalf("round trip:\n got %#v\nwant %#v", decoded, m)
	}
}

func TestDurationJSONRejectsNumbers(t *testing.T) {
	var d Duration
	if err := json.Unmarshal([]byte(`3000`), &d); err == nil {
		t.Fatal("want an error for a number")
	}
}

// TestMeasuredIntervalsCompare checks bursts end to end, from arrivals to differences.
func TestMeasuredIntervalsCompare(t *testing.T) {
	run := func(arrivals []Arrival) Measures {
		return Measure(Input{Sent: []Sent{{At: at(0), Items: []string{"metric:a"}}}, Arrivals: arrivals, Exited: at(41000)}, DefaultTolerances())
	}
	items := []string{"metric:a"}
	tests := []struct {
		name              string
		reference, actual []Arrival
		want              []diff.Difference
	}{
		{
			name:      "one burst per flush on both sides: 3s against 10s fails",
			reference: flushes("series", 3, items, 3000, 6000, 9000, 12000, 15000, 18000, 21000, 24000, 27000, 30000),
			actual:    flushes("series", 2, items, 3000, 13000, 23000, 33000),
			want: []diff.Difference{
				{Area: "timing", Path: "timing.flush_interval.series", Reference: "3s", Actual: "10s"},
				{Area: "timing", Path: "timing.interval.series", Reference: "3s", Actual: "10s"},
			},
		},
		{
			name:      "different burst sizes: 3s against 3s passes",
			reference: flushes("series", 1, items, 3000, 6000, 9000, 12000),
			actual:    flushes("series", 4, items, 3000, 6000, 9000, 12000),
		},
		{
			name:      "the agent's own series every 3s do not hide sent metrics every 10s",
			reference: flushes("series", 1, items, 3000, 6000, 9000, 12000, 15000, 18000, 21000, 24000, 27000, 30000),
			actual: append(flushes("series", 1, []string{"metric:datadog.agent.running"}, 3000, 6000, 9000, 12000, 15000, 18000, 21000, 24000, 27000, 30000),
				flushes("series", 1, items, 3000, 13000, 23000, 33000)...),
			want: []diff.Difference{{Area: "timing", Path: "timing.interval.series", Reference: "3s", Actual: "10s"}},
		},
		{
			// Bucketed metrics arrive about every 10 seconds whether the agent flushes every 3 or
			// every 10, so only the agent's own metrics show the flush interval.
			name: "a 10s flush shows in the agent's own metrics when sent metrics are bucketed",
			reference: append(flushes("series", 1, []string{"metric:datadog.agent.running"}, 3000, 6000, 12000, 15000, 21000, 24000, 30000),
				flushes("series", 1, append([]string{"metric:datadog.agent.running"}, items...), 9000, 18000, 27000)...),
			actual: flushes("series", 1, append([]string{"metric:datadog.agent.running"}, items...), 10000, 20000, 30000),
			want:   []diff.Difference{{Area: "timing", Path: "timing.flush_interval.series", Reference: "3s", Actual: "10s"}},
		},
		{
			name:      "odd versus even burst counts give the same interval",
			reference: flushes("series", 2, items, 3000, 6000, 9000, 12000),
			actual:    flushes("series", 3, items, 3000, 6000, 9000, 12000),
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			checkDifferences(t, Compare(run(test.reference), run(test.actual), DefaultTolerances(), DefaultShutdownLimit), test.want)
		})
	}
}

// base returns measures with nothing measurable and nothing missing.
func base() Measures {
	return Measures{
		IntervalByArea:          allAreas(nil),
		MaxDelayByArea:          allAreas(nil),
		MissingAfterSigterm:     []string{},
		MissingSentAfterSigterm: []string{},
		AmbiguousItems:          []string{},
		Shutdown:                notMeasurable,
	}
}

func withInterval(area string, m Measurement) Measures {
	out := base()
	out.IntervalByArea[area] = m
	return out
}

func withShutdown(d time.Duration) Measures {
	out := base()
	out.Sigterm = true
	out.Shutdown = measured(d)
	return out
}

// sigtermWithoutExit returns measures of a run that got SIGTERM and did not exit.
func sigtermWithoutExit() Measures {
	out := base()
	out.Sigterm = true
	return out
}

// TestDroppedFinalFlushFails checks that a dropped last send of a repeated metric never passes.
func TestDroppedFinalFlushFails(t *testing.T) {
	run := func(first, second string, arrivals ...Arrival) Measures {
		return Measure(Input{
			Sent:     []Sent{{At: at(7000), Items: []string{first}}, {At: at(8000), Items: []string{second}}},
			Arrivals: arrivals,
			Sigterm:  at(9000),
			Exited:   at(10000),
		}, DefaultTolerances())
	}
	tests := []struct {
		name              string
		reference, actual Measures
		want              []diff.Difference
	}{
		{
			name: "distinct tags show the missing send",
			reference: run("metric:m|run:1", "metric:m|run:2",
				Arrival{Area: "series", At: at(8500), Items: []string{"metric:m|host:a,run:1", "metric:m|host:a,run:2"}}),
			actual: run("metric:m|run:1", "metric:m|run:2",
				Arrival{Area: "series", At: at(8500), Items: []string{"metric:m|host:a,run:1"}}),
			want: []diff.Difference{{Area: "timing", Path: "timing.missing_after_sigterm", Reference: []string{}, Actual: []string{"metric:m|run:2"}}},
		},
		{
			name: "an agent tag that completes the second send's tags is reported as ambiguous",
			reference: run("metric:m|route:/", "metric:m|env:x",
				Arrival{Area: "series", At: at(8500), Items: []string{"metric:m|env:x", "metric:m|env:x,route:/"}}),
			// Only the first series arrives, and the agent added env:x to it.
			actual: run("metric:m|route:/", "metric:m|env:x",
				Arrival{Area: "series", At: at(8500), Items: []string{"metric:m|env:x,route:/"}}),
			want: []diff.Difference{{
				Area: "timing", Path: "timing.ambiguous_items",
				Reference: []string{"metric:m|env:x", "metric:m|route:/"}, Actual: []string{"metric:m|env:x", "metric:m|route:/"},
			}},
		},
		{
			name: "identical keys are reported as ambiguous",
			reference: run("metric:m", "metric:m",
				Arrival{Area: "series", At: at(8500), Items: []string{"metric:m|host:a"}}),
			actual: run("metric:m", "metric:m",
				Arrival{Area: "series", At: at(7500), Items: []string{"metric:m|host:a"}}),
			want: []diff.Difference{{Area: "timing", Path: "timing.ambiguous_items", Reference: []string{"metric:m"}, Actual: []string{"metric:m"}}},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			checkDifferences(t, Compare(test.reference, test.actual, DefaultTolerances(), DefaultShutdownLimit), test.want)
		})
	}
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

func TestIntervalThreeVersusTenSecondsFails(t *testing.T) {
	got := Compare(withInterval("series", measured(3*time.Second)), withInterval("series", measured(10*time.Second)), DefaultTolerances(), DefaultShutdownLimit)
	checkDifferences(t, got, []diff.Difference{
		{Area: "timing", Path: "timing.interval.series", Reference: "3s", Actual: "10s"},
	})
}

func TestIntervalWithinOneSecondPasses(t *testing.T) {
	got := Compare(withInterval("series", measured(3*time.Second)), withInterval("series", measured(3600*time.Millisecond)), DefaultTolerances(), DefaultShutdownLimit)
	checkDifferences(t, got, nil)
}

func TestItemSentBeforeSigtermButMissingFails(t *testing.T) {
	actual := base()
	actual.MissingAfterSigterm = []string{"metric:a"}
	got := Compare(base(), actual, DefaultTolerances(), DefaultShutdownLimit)
	checkDifferences(t, got, []diff.Difference{
		{Area: "timing", Path: "timing.missing_after_sigterm", Reference: []string{}, Actual: []string{"metric:a"}},
	})
}

func TestShutdownOverLimitFails(t *testing.T) {
	got := Compare(withShutdown(9200*time.Millisecond), withShutdown(9500*time.Millisecond), DefaultTolerances(), 9*time.Second)
	checkDifferences(t, got, []diff.Difference{
		{Area: "timing", Path: "timing.shutdown_limit", Reference: "9s", Actual: "9.5s"},
	})
}

func TestShutdownSlowerThanGoPlusToleranceFails(t *testing.T) {
	got := Compare(withShutdown(2*time.Second), withShutdown(3500*time.Millisecond), DefaultTolerances(), DefaultShutdownLimit)
	checkDifferences(t, got, []diff.Difference{
		{Area: "timing", Path: "timing.shutdown", Reference: "2s", Actual: "3.5s"},
	})
}

func TestCompare(t *testing.T) {
	withMissing := func(before, after []string) Measures {
		out := base()
		out.MissingAfterSigterm = before
		out.MissingSentAfterSigterm = after
		return out
	}
	withDelay := func(area string, m Measurement) Measures {
		out := base()
		out.MaxDelayByArea[area] = m
		return out
	}
	tests := []struct {
		name              string
		reference, actual Measures
		// limit is the shutdown limit. Zero means the default, 9 seconds.
		limit time.Duration
		want  []diff.Difference
	}{
		{
			name:      "interval measurable only in the reference",
			reference: withInterval("traces", measured(3*time.Second)),
			actual:    base(),
			want:      []diff.Difference{{Area: "timing", Path: "timing.interval.traces", Reference: "3s", Actual: "not measurable"}},
		},
		{
			name:      "interval measurable only in the actual",
			reference: base(),
			actual:    withInterval("traces", measured(3*time.Second)),
			want:      []diff.Difference{{Area: "timing", Path: "timing.interval.traces", Reference: "not measurable", Actual: "3s"}},
		},
		{
			name:      "interval not measurable on both sides is not compared",
			reference: base(),
			actual:    base(),
		},
		{
			name:      "interval exactly one second faster passes",
			reference: withInterval("series", measured(3*time.Second)),
			actual:    withInterval("series", measured(2*time.Second)),
		},
		{
			name:      "interval just over one second faster fails",
			reference: withInterval("series", measured(3*time.Second)),
			actual:    withInterval("series", measured(1999*time.Millisecond)),
			want:      []diff.Difference{{Area: "timing", Path: "timing.interval.series", Reference: "3s", Actual: "1.999s"}},
		},
		{
			name:      "max delay outside the tolerance fails",
			reference: withDelay("logs", measured(1*time.Second)),
			actual:    withDelay("logs", measured(9*time.Second)),
			want:      []diff.Difference{{Area: "timing", Path: "timing.max_delay.logs", Reference: "1s", Actual: "9s"}},
		},
		{
			name:      "max delay within the tolerance passes",
			reference: withDelay("logs", measured(1*time.Second)),
			actual:    withDelay("logs", measured(1500*time.Millisecond)),
		},
		{
			name:      "an area missing from the actual map is not measurable",
			reference: withDelay("logs", measured(1*time.Second)),
			actual:    Measures{MissingAfterSigterm: []string{}, MissingSentAfterSigterm: []string{}},
			want:      []diff.Difference{{Area: "timing", Path: "timing.max_delay.logs", Reference: "1s", Actual: "not measurable"}},
		},
		{
			name:      "reference also misses the item: equal sets pass",
			reference: withMissing([]string{"metric:a"}, []string{}),
			actual:    withMissing([]string{"metric:a"}, []string{}),
		},
		{
			name:      "actual delivers an item the reference missed: passes",
			reference: withMissing([]string{"metric:a", "metric:b"}, []string{}),
			actual:    withMissing([]string{"metric:a"}, []string{}),
		},
		{
			name:      "actual misses an item the reference delivered, beside a shared missing item",
			reference: withMissing([]string{"metric:a"}, []string{}),
			actual:    withMissing([]string{"metric:a", "metric:b"}, []string{}),
			want: []diff.Difference{{
				Area: "timing", Path: "timing.missing_after_sigterm",
				Reference: []string{"metric:a"}, Actual: []string{"metric:a", "metric:b"},
			}},
		},
		{
			name:      "an item sent after SIGTERM that the reference delivered and the actual missed fails",
			reference: withMissing([]string{}, []string{}),
			actual:    withMissing([]string{}, []string{"log:after"}),
			want: []diff.Difference{{
				Area: "timing", Path: "timing.missing_sent_after_sigterm",
				Reference: []string{}, Actual: []string{"log:after"},
			}},
		},
		{
			name:      "shutdown within reference plus tolerance passes",
			reference: withShutdown(2 * time.Second),
			actual:    withShutdown(3 * time.Second),
		},
		{
			name:      "shutdown faster than the reference passes",
			reference: withShutdown(5 * time.Second),
			actual:    withShutdown(1 * time.Second),
		},
		{
			name:      "shutdown over the reference and over the limit gives both differences",
			reference: withShutdown(2 * time.Second),
			actual:    withShutdown(10 * time.Second),
			want: []diff.Difference{
				{Area: "timing", Path: "timing.shutdown", Reference: "2s", Actual: "10s"},
				{Area: "timing", Path: "timing.shutdown_limit", Reference: "9s", Actual: "10s"},
			},
		},
		{
			name:      "a zero limit uses the default limit",
			reference: withShutdown(8500 * time.Millisecond),
			actual:    withShutdown(9500 * time.Millisecond),
			want:      []diff.Difference{{Area: "timing", Path: "timing.shutdown_limit", Reference: "9s", Actual: "9.5s"}},
		},
		{
			name:      "a scenario limit replaces the default",
			reference: withShutdown(8500 * time.Millisecond),
			actual:    withShutdown(9500 * time.Millisecond),
			limit:     12 * time.Second,
		},
		{
			name:      "actual never exited after SIGTERM fails",
			reference: withShutdown(2 * time.Second),
			actual:    sigtermWithoutExit(),
			want: []diff.Difference{
				{Area: "timing", Path: "timing.shutdown", Reference: "2s", Actual: "not measurable"},
				{Area: "timing", Path: "timing.shutdown_limit", Reference: "9s", Actual: "not measurable"},
			},
		},
		{
			name:      "neither side exited after SIGTERM fails against the limit",
			reference: sigtermWithoutExit(),
			actual:    sigtermWithoutExit(),
			want:      []diff.Difference{{Area: "timing", Path: "timing.shutdown_limit", Reference: "9s", Actual: "not measurable"}},
		},
		{
			name:      "ambiguous items fail",
			reference: base(),
			actual: func() Measures {
				m := base()
				m.AmbiguousItems = []string{"log:x"}
				return m
			}(),
			want: []diff.Difference{{Area: "timing", Path: "timing.ambiguous_items", Reference: []string{}, Actual: []string{"log:x"}}},
		},
		{
			name:      "no SIGTERM on either side does not compare shutdown",
			reference: base(),
			actual:    base(),
		},
		{
			name: "differences are sorted by path",
			reference: func() Measures {
				m := withShutdown(1 * time.Second)
				m.IntervalByArea["traces"] = measured(3 * time.Second)
				m.IntervalByArea["series"] = measured(3 * time.Second)
				m.MaxDelayByArea["logs"] = measured(1 * time.Second)
				return m
			}(),
			actual: func() Measures {
				m := withShutdown(5 * time.Second)
				m.IntervalByArea["traces"] = measured(10 * time.Second)
				m.IntervalByArea["series"] = measured(10 * time.Second)
				m.MaxDelayByArea["logs"] = measured(5 * time.Second)
				m.MissingAfterSigterm = []string{"metric:a"}
				m.AmbiguousItems = []string{"log:x"}
				return m
			}(),
			want: []diff.Difference{
				{Area: "timing", Path: "timing.ambiguous_items", Reference: []string{}, Actual: []string{"log:x"}},
				{Area: "timing", Path: "timing.interval.series", Reference: "3s", Actual: "10s"},
				{Area: "timing", Path: "timing.interval.traces", Reference: "3s", Actual: "10s"},
				{Area: "timing", Path: "timing.max_delay.logs", Reference: "1s", Actual: "5s"},
				{Area: "timing", Path: "timing.missing_after_sigterm", Reference: []string{}, Actual: []string{"metric:a"}},
				{Area: "timing", Path: "timing.shutdown", Reference: "1s", Actual: "5s"},
			},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			checkDifferences(t, Compare(test.reference, test.actual, DefaultTolerances(), test.limit), test.want)
		})
	}
}
