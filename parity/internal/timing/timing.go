package timing

import (
	"encoding/json"
	"fmt"
	"slices"
	"sort"
	"strings"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/diff"
)

// DefaultShutdownLimit is the longest shutdown a scenario allows when it sets no limit.
const DefaultShutdownLimit = 9 * time.Second

// Areas are the payload types that the timing check measures. The workload never sends check
// runs, so only their flush interval is measurable.
var Areas = []string{"series", "sketches", "logs", "traces", "stats", "check_runs"}

// minimumFlushes is how many flushes an interval needs, so that it is the median of at least two
// gaps and one late flush cannot decide it.
const minimumFlushes = 3

// Duration is a time.Duration that serializes as a Go duration string, for example "3.5s".
type Duration time.Duration

func (d Duration) String() string { return time.Duration(d).String() }

func (d Duration) MarshalJSON() ([]byte, error) { return json.Marshal(d.String()) }

func (d *Duration) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return fmt.Errorf("duration must be a string such as \"3s\": %w", err)
	}
	parsed, err := time.ParseDuration(text)
	if err != nil {
		return err
	}
	*d = Duration(parsed)
	return nil
}

// Measurement is one timing value. Measurable is false when the run gave too little data for
// it, and Value is then zero and has no meaning.
type Measurement struct {
	Measurable bool     `json:"measurable"`
	Value      Duration `json:"value,omitempty"`
}

func (m Measurement) String() string {
	if !m.Measurable {
		return "not measurable"
	}
	return m.Value.String()
}

// Measures are the timing values of one run. Measure fills both maps with every area.
type Measures struct {
	// IntervalByArea is the median gap between the starts of consecutive flushes of an area.
	// It counts only arrivals that carry an item the workload sent, from the first send until
	// SIGTERM, or until exit when no SIGTERM was sent, and groups them into flushes by
	// Tolerances.FlushGap. It needs minimumFlushes flushes.
	IntervalByArea map[string]Measurement `json:"interval_by_area"`
	// FlushIntervalByArea is the same median over every arrival of an area, the agent's own
	// metrics included. Go puts the workload's DogStatsD metrics into 10-second buckets, so
	// their interval hardly changes with the flush interval, which the agent's own metrics,
	// sent at every flush, show. It lists only the areas where it is measurable.
	FlushIntervalByArea map[string]Measurement `json:"flush_interval_by_area,omitempty"`
	// MaxDelayByArea is the longest time from sending an item until the first arrival of that
	// area, at or after the send, that carries it. Items that never arrive in the area do not
	// count, so it is not measurable when no sent item arrived in the area.
	MaxDelayByArea map[string]Measurement `json:"max_delay_by_area"`
	// MissingAfterSigterm lists, sorted, the items sent before SIGTERM that arrived in no area.
	// When no SIGTERM was sent, every sent item counts as sent before it.
	MissingAfterSigterm []string `json:"missing_after_sigterm"`
	// MissingSentAfterSigterm lists, sorted, the items sent at or after SIGTERM that arrived
	// in no area.
	MissingSentAfterSigterm []string `json:"missing_sent_after_sigterm"`
	// AmbiguousItems lists, sorted, the sent items that one arrival could deliver for more
	// than one send, so their delay and delivery are unknown. Delay and missing skip them.
	AmbiguousItems []string `json:"ambiguous_items"`
	// Sigterm is true when the harness sent SIGTERM.
	Sigterm bool `json:"sigterm"`
	// Shutdown is the time from SIGTERM to exit. It is not measurable without a SIGTERM, or
	// when the process did not exit after it.
	Shutdown Measurement `json:"shutdown"`
}

// Measure computes the timing values of one run. It uses only tol.FlushGap.
//
// An arrival delivers a sent item when it is of the same kind, it is at or after the send and,
// when the process exited, at or before the exit, and it carries the item: a metric with the
// same name and every sent tag, since the agent adds its own tags, or the same log message or
// span resource.
func Measure(in Input, tol Tolerances) Measures {
	windowEnd := in.Sigterm
	if windowEnd.IsZero() {
		windowEnd = in.Exited
	}
	var firstSent time.Time
	for _, sent := range in.Sent {
		if firstSent.IsZero() || sent.At.Before(firstSent) {
			firstSent = sent.At
		}
	}

	measures := Measures{
		IntervalByArea:          map[string]Measurement{},
		MaxDelayByArea:          map[string]Measurement{},
		MissingAfterSigterm:     []string{},
		MissingSentAfterSigterm: []string{},
		AmbiguousItems:          ambiguousItems(in.Sent, in.Arrivals),
		Sigterm:                 !in.Sigterm.IsZero(),
	}
	for _, area := range Areas {
		var sentTimes, allTimes []time.Time
		if !firstSent.IsZero() {
			for _, arrival := range in.Arrivals {
				if arrival.Area == area && !arrival.At.Before(firstSent) && arrival.At.Before(windowEnd) {
					allTimes = append(allTimes, arrival.At)
					if carriesSent(arrival, in.Sent) {
						sentTimes = append(sentTimes, arrival.At)
					}
				}
			}
		}
		measures.IntervalByArea[area] = flushInterval(sentTimes, tol.FlushGap)
		if all := flushInterval(allTimes, tol.FlushGap); all.Measurable {
			if measures.FlushIntervalByArea == nil {
				measures.FlushIntervalByArea = map[string]Measurement{}
			}
			measures.FlushIntervalByArea[area] = all
		}
		measures.MaxDelayByArea[area] = Measurement{}
	}

	for _, sent := range in.Sent {
		for _, item := range sent.Items {
			if slices.Contains(measures.AmbiguousItems, item) {
				continue
			}
			delivered := false
			for _, area := range Areas {
				arrived, ok := firstArrival(in.Arrivals, area, item, sent.At, in.Exited)
				if !ok {
					continue
				}
				delivered = true
				delay := Duration(arrived.Sub(sent.At))
				if current := measures.MaxDelayByArea[area]; !current.Measurable || delay > current.Value {
					measures.MaxDelayByArea[area] = Measurement{Measurable: true, Value: delay}
				}
			}
			if delivered {
				continue
			}
			if in.Sigterm.IsZero() || sent.At.Before(in.Sigterm) {
				measures.MissingAfterSigterm = append(measures.MissingAfterSigterm, item)
			} else {
				measures.MissingSentAfterSigterm = append(measures.MissingSentAfterSigterm, item)
			}
		}
	}
	measures.MissingAfterSigterm = sortedUnique(measures.MissingAfterSigterm)
	measures.MissingSentAfterSigterm = sortedUnique(measures.MissingSentAfterSigterm)

	if !in.Sigterm.IsZero() && !in.Exited.IsZero() && !in.Exited.Before(in.Sigterm) {
		measures.Shutdown = Measurement{Measurable: true, Value: Duration(in.Exited.Sub(in.Sigterm))}
	}
	return measures
}

// carriesSent reports whether the arrival holds at least one item that the workload sent. The
// interval is measured on those arrivals only, so that the agent's own metrics, which it sends
// at every flush, can't hide a slower flush of the workload's.
func carriesSent(arrival Arrival, sent []Sent) bool {
	for _, arrived := range arrival.Items {
		for _, s := range sent {
			for _, item := range s.Items {
				if matches(item, arrived) {
					return true
				}
			}
		}
	}
	return false
}

// flushInterval groups the times into flushes, where a time less than flushGap after the
// previous one joins its flush, and returns the median gap between flush starts. It is not
// measurable for fewer than minimumFlushes flushes.
func flushInterval(times []time.Time, flushGap time.Duration) Measurement {
	sort.Slice(times, func(i, j int) bool { return times[i].Before(times[j]) })
	var starts []time.Time
	for i, t := range times {
		if i == 0 || !(t.Sub(times[i-1]) < flushGap) {
			starts = append(starts, t)
		}
	}
	if len(starts) < minimumFlushes {
		return Measurement{}
	}
	gaps := make([]time.Duration, 0, len(starts)-1)
	for i := 1; i < len(starts); i++ {
		gaps = append(gaps, starts[i].Sub(starts[i-1]))
	}
	slices.Sort(gaps)
	middle := len(gaps) / 2
	median := gaps[middle]
	if len(gaps)%2 == 0 {
		median = (gaps[middle-1] + gaps[middle]) / 2
	}
	return Measurement{Measurable: true, Value: Duration(median)}
}

// firstArrival returns the time of the earliest arrival of the area that delivers the item.
func firstArrival(arrivals []Arrival, area, item string, sentAt, exited time.Time) (time.Time, bool) {
	var first time.Time
	found := false
	for _, arrival := range arrivals {
		if arrival.Area != area || arrival.At.Before(sentAt) || (!exited.IsZero() && arrival.At.After(exited)) ||
			(found && !arrival.At.Before(first)) {
			continue
		}
		if slices.ContainsFunc(arrival.Items, func(arrived string) bool { return matches(item, arrived) }) {
			first, found = arrival.At, true
		}
	}
	return first, found
}

// matches reports whether an arrived item key carries a sent item key.
func matches(sent, arrived string) bool {
	sentName, sentTags, sentIsMetric := splitMetricKey(sent)
	arrivedName, arrivedTags, arrivedIsMetric := splitMetricKey(arrived)
	if !sentIsMetric || !arrivedIsMetric {
		return sent == arrived
	}
	if sentName != arrivedName {
		return false
	}
	for _, tag := range sentTags {
		if !slices.Contains(arrivedTags, tag) {
			return false
		}
	}
	return true
}

// splitMetricKey returns the name and tags of a MetricKey, and false for other keys.
func splitMetricKey(key string) (string, []string, bool) {
	rest, isMetric := strings.CutPrefix(key, "metric:")
	if !isMetric {
		return "", nil, false
	}
	name, tags, hasTags := strings.Cut(rest, "|")
	if !hasTags {
		return name, nil, true
	}
	return name, strings.Split(tags, ","), true
}

// ambiguousItems returns, sorted, the sent item keys that one arrival could deliver for more
// than one send: the same key sent in more than one batch, two sent keys of which one matches
// the other, such as a metric and the same metric with more tags, and keys that one arrived
// item matches together, such as m|route:/ and m|env:x when the agent adds env:x.
func ambiguousItems(sent []Sent, arrivals []Arrival) []string {
	type occurrence struct {
		batch int
		item  string
	}
	var occurrences []occurrence
	for batch, s := range sent {
		for _, item := range s.Items {
			if !slices.Contains(occurrences, occurrence{batch, item}) {
				occurrences = append(occurrences, occurrence{batch, item})
			}
		}
	}
	ambiguous := []string{}
	for i, a := range occurrences {
		for _, b := range occurrences[i+1:] {
			if matches(a.item, b.item) || matches(b.item, a.item) {
				ambiguous = append(ambiguous, a.item, b.item)
			}
		}
	}
	for _, arrival := range arrivals {
		for _, arrived := range arrival.Items {
			var matched []string
			for _, o := range occurrences {
				if matches(o.item, arrived) {
					matched = append(matched, o.item)
				}
			}
			if len(matched) > 1 {
				ambiguous = append(ambiguous, matched...)
			}
		}
	}
	return sortedUnique(ambiguous)
}

func sortedUnique(items []string) []string {
	slices.Sort(items)
	return slices.Compact(items)
}

// Compare returns the timing differences of the actual run from the reference run, sorted by
// path. A shutdown limit of zero means DefaultShutdownLimit. Every check fails closed: a
// value passes only when the comparison holds.
func Compare(reference, actual Measures, tol Tolerances, shutdownLimit time.Duration) []diff.Difference {
	if shutdownLimit <= 0 {
		shutdownLimit = DefaultShutdownLimit
	}
	var differences []diff.Difference
	add := func(path string, referenceValue, actualValue any) {
		differences = append(differences, diff.Difference{Area: "timing", Path: path, Reference: referenceValue, Actual: actualValue})
	}

	for _, area := range Areas {
		if !withinTolerance(reference.IntervalByArea[area], actual.IntervalByArea[area], tol.Interval) {
			add("timing.interval."+area, reference.IntervalByArea[area].String(), actual.IntervalByArea[area].String())
		}
		if !withinTolerance(reference.FlushIntervalByArea[area], actual.FlushIntervalByArea[area], tol.Interval) {
			add("timing.flush_interval."+area, reference.FlushIntervalByArea[area].String(), actual.FlushIntervalByArea[area].String())
		}
		if !withinTolerance(reference.MaxDelayByArea[area], actual.MaxDelayByArea[area], tol.Delay) {
			add("timing.max_delay."+area, reference.MaxDelayByArea[area].String(), actual.MaxDelayByArea[area].String())
		}
	}

	if len(reference.AmbiguousItems) > 0 || len(actual.AmbiguousItems) > 0 {
		add("timing.ambiguous_items", nonNil(reference.AmbiguousItems), nonNil(actual.AmbiguousItems))
	}
	if missesMore(reference.MissingAfterSigterm, actual.MissingAfterSigterm) {
		add("timing.missing_after_sigterm", nonNil(reference.MissingAfterSigterm), nonNil(actual.MissingAfterSigterm))
	}
	if missesMore(reference.MissingSentAfterSigterm, actual.MissingSentAfterSigterm) {
		add("timing.missing_sent_after_sigterm", nonNil(reference.MissingSentAfterSigterm), nonNil(actual.MissingSentAfterSigterm))
	}

	if reference.Shutdown.Measurable || actual.Shutdown.Measurable {
		if !(reference.Shutdown.Measurable && actual.Shutdown.Measurable &&
			time.Duration(actual.Shutdown.Value) <= time.Duration(reference.Shutdown.Value)+tol.Shutdown) {
			add("timing.shutdown", reference.Shutdown.String(), actual.Shutdown.String())
		}
	}
	if actual.Sigterm || actual.Shutdown.Measurable {
		if !(actual.Shutdown.Measurable && time.Duration(actual.Shutdown.Value) <= shutdownLimit) {
			add("timing.shutdown_limit", shutdownLimit.String(), actual.Shutdown.String())
		}
	}

	sort.SliceStable(differences, func(i, j int) bool { return differences[i].Path < differences[j].Path })
	return differences
}

// withinTolerance reports whether two measurements agree: both not measurable, or both
// measurable and no more than the tolerance apart.
func withinTolerance(reference, actual Measurement, tolerance time.Duration) bool {
	if !reference.Measurable && !actual.Measurable {
		return true
	}
	if reference.Measurable != actual.Measurable {
		return false
	}
	gap := time.Duration(actual.Value) - time.Duration(reference.Value)
	if gap < 0 {
		gap = -gap
	}
	return gap <= tolerance
}

// missesMore reports whether the actual run missed an item that the reference run delivered.
func missesMore(referenceMissing, actualMissing []string) bool {
	for _, item := range actualMissing {
		if !slices.Contains(referenceMissing, item) {
			return true
		}
	}
	return false
}

func nonNil(items []string) []string {
	if items == nil {
		return []string{}
	}
	return items
}
