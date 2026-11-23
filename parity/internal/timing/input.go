// Package timing holds the inputs and tolerances of the flush timing check.
package timing

import (
	"slices"
	"strings"
	"time"
)

// Arrival is one decoded payload that fakeintake received.
type Arrival struct {
	// Area is one of Areas.
	Area string    `json:"area"`
	At   time.Time `json:"at"`
	// Items are the item keys the payload carries, sorted and each listed once: MetricKey for
	// series and sketches, log:<message> for logs, span:<resource> for traces and stats, and
	// check:<name> for check runs.
	Items []string `json:"items"`
}

// Sent is one item batch that the workload driver sent, with item keys as in Arrival.
type Sent struct {
	At    time.Time `json:"at"`
	Items []string  `json:"items"`
}

// Input is the raw timing data of one run. Every time is on the Docker clock.
type Input struct {
	Arrivals []Arrival `json:"arrivals"`
	Sent     []Sent    `json:"sent"`
	// Sigterm is when the harness sent the stop signal, or the driver's first signal event when
	// that is earlier. It is the zero time when the harness sent none.
	Sigterm time.Time `json:"sigterm"`
	Exited  time.Time `json:"exited"`
}

// Tolerances are the allowed differences from Go's timing.
type Tolerances struct {
	// Interval is the allowed difference in the flush interval.
	Interval time.Duration
	// Delay is the allowed difference in the longest delivery delay.
	Delay time.Duration
	// Shutdown is how much longer than Go's the shutdown may take.
	Shutdown time.Duration
	// Percentile is the allowed relative difference of sketch percentiles.
	Percentile float64
	// FlushGap groups arrivals into flushes: an arrival of an area less than FlushGap after the
	// previous one belongs to the same flush.
	FlushGap time.Duration
}

// DefaultTolerances returns the tolerances used when normalize.yaml does not set them.
func DefaultTolerances() Tolerances {
	return Tolerances{Interval: time.Second, Delay: time.Second, Shutdown: time.Second, Percentile: 0.01, FlushGap: 500 * time.Millisecond}
}

// MetricKey returns the item key of a metric: metric:<name>, followed by |<tags> when it has
// tags, with the tags sorted, listed once and joined by commas. DogStatsD separates tags with
// commas and fields with |, so neither appears inside a name or a tag.
func MetricKey(name string, tags []string) string {
	sorted := slices.DeleteFunc(slices.Clone(tags), func(tag string) bool { return tag == "" })
	slices.Sort(sorted)
	sorted = slices.Compact(sorted)
	if len(sorted) == 0 {
		return "metric:" + name
	}
	return "metric:" + name + "|" + strings.Join(sorted, ",")
}
