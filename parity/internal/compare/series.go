package compare

import (
	"math"
	"slices"
	"strconv"
	"strings"

	"github.com/DataDog/serverless-components/parity/internal/normalize"
)

// seriesGroup is the series of one metric, type and tag set, combined.
type seriesGroup struct {
	// Value is the sum of the points of a count or rate and the last point of a gauge.
	Value float64 `json:"value"`
	// Points lists every point value, in order, for the other types.
	Points     []float64           `json:"points,omitempty"`
	Attributes map[string][]string `json:"attributes,omitempty"`
}

func seriesGroups(series []normalize.Series) map[string]any {
	members := map[string][]normalize.Series{}
	var order []string
	for _, s := range series {
		key := "series[" + strings.Join(append([]string{"metric=" + s.Metric, "type=" + s.Type, "tags=" + sortedTags(s.Tags)},
			keyFields("", s, nil, "metric", "type", "tags", "points")...), ",") + "]"
		if _, ok := members[key]; !ok {
			order = append(order, key)
		}
		members[key] = append(members[key], s)
	}
	groups := map[string]any{}
	for _, key := range order {
		var g seriesGroup
		var points []any
		for _, s := range members[key] {
			for _, p := range s.Points {
				points = append(points, p)
				switch s.Type {
				case "count", "rate":
					g.Value += p.Value
				case "gauge":
					g.Value = p.Value
				default:
					g.Points = append(g.Points, p.Value)
				}
			}
		}
		g.Attributes = map[string][]string{}
		attributes(g.Attributes, "points.", points, "value")
		g.Attributes = nilIfEmpty(g.Attributes)
		groups[key] = g
	}
	return groups
}

// sketchGroup is the sketches of one metric and tag set, merged.
type sketchGroup struct {
	Count     int64              `json:"count"`
	Sum       float64            `json:"sum"`
	Min       float64            `json:"min"`
	Max       float64            `json:"max"`
	Quantiles map[string]float64 `json:"quantiles,omitempty"`
	// Attributes hold the per-point fields and the older distribution encoding.
	Attributes map[string][]string `json:"attributes,omitempty"`
}

var quantiles = []struct {
	name     string
	quantile float64
}{{"p50", 0.5}, {"p75", 0.75}, {"p90", 0.9}, {"p95", 0.95}, {"p99", 0.99}}

func sketchGroups(sketches []normalize.Sketch) map[string]any {
	members := map[string][]normalize.Sketch{}
	for _, s := range sketches {
		key := "sketches[" + strings.Join(append([]string{"metric=" + s.Metric, "tags=" + sortedTags(s.Tags)},
			keyFields("", s, nil, "metric", "tags", "points", "distributions")...), ",") + "]"
		members[key] = append(members[key], s)
	}
	groups := map[string]any{}
	for key, list := range members {
		var g sketchGroup
		var objects, points []any
		bins := map[int32]uint64{}
		first := true
		for _, s := range list {
			objects = append(objects, struct {
				Distributions []normalize.Distribution `json:"distributions,omitempty"`
			}{s.Distributions})
			for _, p := range s.Points {
				points = append(points, p)
				for i, k := range p.K {
					if i < len(p.N) {
						bins[k] += uint64(p.N[i])
					}
				}
				g.Count += p.Count
				g.Sum += p.Sum
				if p.Count == 0 {
					continue
				}
				if first || p.Min < g.Min {
					g.Min = p.Min
				}
				if first || p.Max > g.Max {
					g.Max = p.Max
				}
				first = false
			}
		}
		if g.Count > 0 {
			g.Quantiles = map[string]float64{}
			for _, q := range quantiles {
				g.Quantiles[q.name] = sketchQuantile(bins, g.Count, g.Min, g.Max, q.quantile)
			}
		}
		g.Attributes = map[string][]string{}
		attributes(g.Attributes, "", objects)
		attributes(g.Attributes, "points.", points, "count", "min", "max", "avg", "sum", "k", "n")
		g.Attributes = nilIfEmpty(g.Attributes)
		groups[key] = g
	}
	return groups
}

// The agent's default sketch configuration (pkg/util/quantile): a relative accuracy of 1/128
// and a smallest value of 1e-9.
var (
	sketchGamma = 1 + 2.0/128
	sketchBias  = -int(math.Floor(math.Log(1e-9)/math.Log(sketchGamma))) + 1
)

// sketchQuantile is the agent's Sketch.Quantile over merged bins: it interpolates in the bin
// that holds the wanted rank, with the minimum as the low edge of the first bin.
func sketchQuantile(bins map[int32]uint64, count int64, minimum, maximum, q float64) float64 {
	keys := make([]int32, 0, len(bins))
	for k := range bins {
		keys = append(keys, k)
	}
	slices.Sort(keys)
	want := math.RoundToEven(q * float64(count-1))
	var n float64
	for i, k := range keys {
		n += float64(bins[k])
		if n <= want {
			continue
		}
		weight := (n - want) / float64(bins[k])
		low := sketchValue(k)
		high := low * sketchGamma
		if i == 0 {
			low = minimum
		}
		if weight == 1 {
			// Skips high, which is infinite in the infinite bin.
			return low
		}
		return low*weight + high*(1-weight)
	}
	return maximum
}

// sketchInfiniteKey is the agent's key for values too large for the sketch.
const sketchInfiniteKey = 1<<15 - 1

func sketchValue(k int32) float64 {
	switch {
	case k < 0:
		return -sketchValue(-k)
	case k == 0:
		return 0
	case k >= sketchInfiniteKey:
		return math.Inf(1)
	}
	return math.Pow(sketchGamma, float64(int(k)-sketchBias))
}

// checkRunGroup is the check runs of one check, status, tag set and message. The agent sends a
// check run at every flush, so how many arrive depends on the run's length and is not compared.
type checkRunGroup struct {
	// Attributes hold the per-run fields outside the key, such as the masked timestamp.
	Attributes map[string][]string `json:"attributes,omitempty"`
}

func checkRunGroups(runs []normalize.CheckRun) map[string]any {
	members := map[string][]any{}
	for _, r := range runs {
		key := "check_runs[" + strings.Join(append([]string{"check=" + r.Check, "status=" + strconv.Itoa(r.Status), "tags=" + sortedTags(r.Tags)},
			keyFields("", r, nil, "check", "status", "tags", "timestamp")...), ",") + "]"
		members[key] = append(members[key], r)
	}
	groups := map[string]any{}
	for key, list := range members {
		g := checkRunGroup{Attributes: map[string][]string{}}
		attributes(g.Attributes, "", list, "check", "status", "tags", "host_name", "message")
		g.Attributes = nilIfEmpty(g.Attributes)
		groups[key] = g
	}
	return groups
}
