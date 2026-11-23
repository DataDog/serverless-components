package normalize

import (
	"cmp"
	"fmt"
	"slices"
	"strings"
	"time"

	"github.com/DataDog/datadog-agent/test/fakeintake/aggregator"

	"github.com/DataDog/serverless-components/parity/internal/capture"
	"github.com/DataDog/serverless-components/parity/internal/events"
	"github.com/DataDog/serverless-components/parity/internal/timing"
	"github.com/DataDog/serverless-components/parity/internal/workload"
)

// clockOffsetError bounds the error of the measured clock offset. It is half the time a docker
// exec takes, which is under 100 milliseconds on the machines the harness runs on.
const clockOffsetError = 100 * time.Millisecond

// timingInput extracts the arrivals of the decoded payloads, ordered by arrival time, the
// driver's sends and the run's milestones from c, with every time on the Docker clock.
func timingInput(c capture.Capture) (timing.Input, error) {
	in := timing.Input{Exited: c.Outcome.ExitedAt}
	if !c.Outcome.SigtermAt.IsZero() {
		// The clock offset is an estimate, which can put the stop after a fast exit. The driver
		// receives the signal no earlier than the agent: in sidecar mode the harness signals
		// both, and in-container the agent forwards it. So the driver's event of the stop signal
		// is the better stop time when it is a little earlier than the estimate, within the
		// estimate's error. A later one is kept out, so that an agent that forwards late does not
		// shorten its own shutdown time.
		in.Sigterm = c.Outcome.SigtermAt.Add(c.Outcome.ClockOffset)
		for _, e := range c.Events {
			if e.Kind != "signal" || e.Detail["signal"] != c.Outcome.StopSignal {
				continue
			}
			if e.Time.Before(in.Sigterm) && in.Sigterm.Sub(e.Time) <= clockOffsetError {
				in.Sigterm = e.Time
			}
			break
		}
	}
	for _, p := range c.Payloads {
		if p.Error != "" {
			continue
		}
		if area, items := arrivalItems(p.Decoded); area != "" {
			slices.Sort(items)
			in.Arrivals = append(in.Arrivals, timing.Arrival{Area: area, At: p.ArrivedAt, Items: items})
		}
	}
	slices.SortStableFunc(in.Arrivals, func(a, b timing.Arrival) int {
		return cmp.Or(a.At.Compare(b.At), strings.Compare(a.Area, b.Area))
	})
	for _, e := range c.Events {
		items, err := sentItems(e)
		if err != nil {
			return timing.Input{}, err
		}
		if len(items) > 0 {
			in.Sent = append(in.Sent, timing.Sent{At: e.Time, Items: items})
		}
	}
	return in, nil
}

// arrivalItems returns the area of a decoded payload and its item keys, each listed once. Stats carry the
// resources of the spans they count, so a span's delivery can be timed in both areas.
func arrivalItems(decoded any) (string, []string) {
	var items itemSet
	switch d := decoded.(type) {
	case []*aggregator.MetricSeries:
		for _, s := range d {
			items.add(timing.MetricKey(s.Metric, s.Tags))
		}
		return "series", items
	case []*aggregator.Sketch:
		for _, s := range d {
			items.add(timing.MetricKey(s.Metric, s.Tags))
		}
		return "sketches", items
	case []*aggregator.Log:
		for _, l := range d {
			items.add("log:" + l.Message)
		}
		return "logs", items
	case []*aggregator.TracePayload:
		for _, t := range d {
			if t.AgentPayload == nil {
				continue
			}
			for _, tp := range t.TracerPayloads {
				for _, c := range tp.Chunks {
					for _, s := range c.Spans {
						items.add("span:" + s.Resource)
					}
				}
			}
			for _, tp := range t.IdxTracerPayloads {
				for _, resource := range indexedResources(tp) {
					items.add("span:" + resource)
				}
			}
		}
		return "traces", items
	case []*aggregator.APMStatsPayload:
		for _, p := range d {
			if p.StatsPayload == nil {
				continue
			}
			for _, cs := range p.Stats {
				for _, b := range cs.Stats {
					for _, g := range b.Stats {
						items.add("span:" + g.Resource)
					}
				}
			}
		}
		return "stats", items
	case []*aggregator.CheckRun:
		for _, r := range d {
			items.add("check:" + r.Check)
		}
		return "check_runs", items
	}
	return "", nil
}

// sentItems returns the item keys that a driver event sent. Sends that failed, and steps that
// send nothing the intake receives, have none. Printed and appended lines are log messages as
// the agent sends them: the driver adds the child marker to printed lines, and writes appended
// lines unchanged.
func sentItems(e events.Event) ([]string, error) {
	var items itemSet
	switch e.Kind {
	case "printed":
		if line, ok := e.Detail["line"].(string); ok {
			items.add("log:" + line)
		}
		return items, nil
	case "sent":
	default:
		return nil, nil
	}
	if _, failed := e.Detail["error"]; failed {
		return nil, nil
	}
	switch e.Detail["step"] {
	case "send_dogstatsd":
		for _, line := range stringList(e.Detail["lines"]) {
			// Service checks and events are not metrics.
			if strings.HasPrefix(line, "_sc|") || strings.HasPrefix(line, "_e{") {
				continue
			}
			if name, _, found := strings.Cut(line, ":"); found && name != "" {
				items.add(timing.MetricKey(name, dogstatsdTags(line)))
			}
		}
	case "send_spans":
		fixture, _ := e.Detail["fixture"].(string)
		resources, err := workload.FixtureResources(fixture)
		if err != nil {
			return nil, fmt.Errorf("reading the resources of a sent fixture: %w", err)
		}
		for _, resource := range resources {
			items.add("span:" + resource)
		}
	case "append_log":
		for _, line := range stringList(e.Detail["lines"]) {
			items.add("log:" + line)
		}
	}
	return items, nil
}

// dogstatsdTags returns the tags of a DogStatsD metric line: the comma-separated list in the
// field that starts with #.
func dogstatsdTags(line string) []string {
	for _, field := range strings.Split(line, "|")[1:] {
		if tags, found := strings.CutPrefix(field, "#"); found {
			return strings.Split(tags, ",")
		}
	}
	return nil
}

// itemSet is a list of item keys that keeps the first occurrence of each.
type itemSet []string

func (s *itemSet) add(item string) {
	for _, existing := range *s {
		if existing == item {
			return
		}
	}
	*s = append(*s, item)
}
