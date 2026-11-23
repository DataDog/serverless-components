// Package compare compares two normalized captures by what a customer would see, as in the
// table in design section 5.2, and marks the differences that expected-differences.yaml allows.
//
// Each content area is combined into items, and each item has a key. A Difference path is the
// item key followed by the field, in the dotted form of the normalizer rules: a struct field is
// .name, a map entry is [key] and a list element is [index]. The item keys are:
//
//   - series[metric=M,type=T,tags=a:1|b:2,...]: the series of one metric, type and sorted tag
//     set, followed by every other non-empty field outside the points (interval, unit,
//     source_type_name, host, resources, origin), which batching can't change. Its value is the
//     sum of every point for counts and rates and the last point for gauges; other types list
//     every point value in order.
//   - sketches[metric=M,tags=a:1|b:2,...]: the merged sketches of one metric and sorted tag set,
//     followed by every other non-empty field outside the points and distributions.
//   - logs[ddsource=S,index=I]: the I-th log of one source, counted from 0.
//   - traces[priority=P,service=S,name=N,resource=R]: one trace chunk, named by its priority and
//     root span. It holds the chunk and the fields of the tracer payload and the agent payload
//     that carried it. When several unmatched chunks share a name, ",index=I" is added.
//   - stats[service=S,name=N,resource=R,...]: the stats of one group, merged over buckets and
//     payloads. The key lists every non-empty grouping field, then the tracer's fields with the
//     prefix client. and the payload's fields with the prefix payload. Latency summaries are
//     merged bin by bin, and their quantiles computed from the merged sketch.
//   - check_runs[check=C,status=S,tags=a:1|b:2,...]: the service check runs of one check, status
//     and sorted tag set, followed by the other non-empty fields except the timestamp (host_name,
//     message). The agent sends a check run at every flush, so their number is not compared.
//   - process: the process view.
//   - undecoded[route=R,error=E]: one payload that was not compared.
//
// Per-point and per-bucket fields that the combination does not sum or merge, such as point
// timestamps, are compared as attributes: for each field, the sorted set of the JSON values it
// takes in the item, with "<absent>" for an item part that leaves it out.
//
// A path pattern, in an override or an expected difference, is a path in which * matches any
// run of characters other than "]". So "series[metric=m,*].value" matches the value of every
// series of metric m, and "traces[priority=0,*]" matches whole priority-0 chunks but no field
// inside one.
package compare

import (
	"encoding/json"
	"fmt"
	"math"
	"reflect"
	"slices"
	"sort"
	"strconv"
	"strings"

	"github.com/DataDog/serverless-components/parity/internal/diff"
	"github.com/DataDog/serverless-components/parity/internal/normalize"
	"github.com/DataDog/serverless-components/parity/internal/scenario"
	"github.com/DataDog/serverless-components/parity/internal/timing"
)

// The statuses of a scenario. The runner reports harness-error when the run itself failed.
const (
	StatusPass         = "pass"
	StatusFail         = "fail"
	StatusExpectedOnly = "expected-only"
	StatusNoReference  = "no-reference"
	StatusHarnessError = "harness-error"
	// StatusReported is the status of a report-only scenario that ran: its differences are
	// listed, but it neither passes nor fails.
	StatusReported = "reported"
)

// absent stands for a field that one part of an item leaves out.
const absent = "<absent>"

// Content compares the content of two captures and returns every difference, sorted by area
// and then by path. Undecoded payloads are always differences, because a payload that was not
// compared is never a pass. Timing is compared elsewhere.
func Content(reference, actual normalize.Normalized, overrides []scenario.Override, tol timing.Tolerances) []diff.Difference {
	var differences []diff.Difference
	referenceTraces, actualTraces := traceItems(reference.Traces, actual.Traces)
	referenceStats, actualStats := statsGroups(reference.Stats), statsGroups(actual.Stats)
	for _, area := range []struct {
		name              string
		reference, actual map[string]any
	}{
		{"series", seriesGroups(reference.Series), seriesGroups(actual.Series)},
		{"sketches", sketchGroups(reference.Sketches), sketchGroups(actual.Sketches)},
		{"logs", logItems(reference.Logs), logItems(actual.Logs)},
		{"traces", referenceTraces, actualTraces},
		{"stats", referenceStats, actualStats},
		{"check_runs", checkRunGroups(reference.CheckRuns), checkRunGroups(actual.CheckRuns)},
		{"process", map[string]any{"process": reference.Process}, map[string]any{"process": actual.Process}},
	} {
		c := comparer{area: area.name, overrides: overrides, tolerances: tol}
		c.compareItems(area.reference, area.actual)
		differences = append(differences, c.differences...)
	}
	differences = append(differences, unmergedSummaries(referenceStats, actualStats, differences)...)
	differences = append(differences, undecodedDifferences(reference.Undecoded, actual.Undecoded)...)
	sort.SliceStable(differences, func(i, j int) bool {
		a, b := differences[i], differences[j]
		if a.Area != b.Area {
			return a.Area < b.Area
		}
		return naturalLess(a.Path, b.Path)
	})
	return differences
}

// Status returns the status of a scenario. Without a reference capture there is nothing to
// compare with, which is never a pass. An undecoded payload always fails, even if marked
// expected, because its content was not compared.
func Status(reference *normalize.Normalized, diffs []diff.Difference) string {
	if reference == nil {
		return StatusNoReference
	}
	if len(diffs) == 0 {
		return StatusPass
	}
	for _, d := range diffs {
		if !d.Expected || d.Area == "undecoded" {
			return StatusFail
		}
	}
	return StatusExpectedOnly
}

// comparer walks two values of the same type and collects their differences.
type comparer struct {
	area        string
	overrides   []scenario.Override
	tolerances  timing.Tolerances
	differences []diff.Difference
}

func (c *comparer) add(path string, reference, actual any) {
	c.differences = append(c.differences, diff.Difference{Area: c.area, Path: path, Reference: reference, Actual: actual})
}

// compareItems compares items by key. An item on one side only is one difference.
func (c *comparer) compareItems(reference, actual map[string]any) {
	keys := map[string]bool{}
	for k := range reference {
		keys[k] = true
	}
	for k := range actual {
		keys[k] = true
	}
	for _, k := range sortedKeys(keys) {
		r, inReference := reference[k]
		a, inActual := actual[k]
		if !inReference || !inActual {
			c.add(k, r, a)
			continue
		}
		c.compare(k, reflect.ValueOf(r), reflect.ValueOf(a))
	}
}

// compare compares two values. An invalid value is a missing map entry.
func (c *comparer) compare(path string, reference, actual reflect.Value) {
	if !reference.IsValid() || !actual.IsValid() {
		if reference.IsValid() || actual.IsValid() {
			c.add(path, plain(reference), plain(actual))
		}
		return
	}
	if o, ok := c.override(path); ok {
		if !overrideHolds(o, plain(reference), plain(actual)) {
			c.add(path, plain(reference), plain(actual))
		}
		return
	}
	switch reference.Kind() {
	case reflect.Pointer, reflect.Interface:
		if reference.IsNil() || actual.IsNil() {
			if !reference.IsNil() || !actual.IsNil() {
				c.add(path, plain(reference), plain(actual))
			}
			return
		}
		c.compare(path, reference.Elem(), actual.Elem())
	case reflect.Struct:
		for i := 0; i < reference.NumField(); i++ {
			name := fieldName(reference.Type().Field(i))
			if name == "" {
				continue
			}
			c.compare(path+"."+name, reference.Field(i), actual.Field(i))
		}
	case reflect.Map:
		keys := map[string]reflect.Value{}
		for _, k := range append(reference.MapKeys(), actual.MapKeys()...) {
			keys[fmt.Sprint(k.Interface())] = k
		}
		for _, name := range sortedKeys(keys) {
			c.compare(path+"["+name+"]", reference.MapIndex(keys[name]), actual.MapIndex(keys[name]))
		}
	case reflect.Slice, reflect.Array:
		if reference.Len() != actual.Len() {
			c.add(path, plain(reference), plain(actual))
			return
		}
		for i := 0; i < reference.Len(); i++ {
			c.compare(path+"["+strconv.Itoa(i)+"]", reference.Index(i), actual.Index(i))
		}
	case reflect.Float32, reflect.Float64:
		r, a := reference.Float(), actual.Float()
		if isQuantile(path) {
			// Written so that NaN fails.
			if !(math.Abs(a-r) <= c.tolerances.Percentile*math.Abs(r)) {
				c.add(path, plain(reference), plain(actual))
			}
		} else if r != a {
			c.add(path, plain(reference), plain(actual))
		}
	default:
		if !reflect.DeepEqual(reference.Interface(), actual.Interface()) {
			c.add(path, plain(reference), plain(actual))
		}
	}
}

// isQuantile reports whether path is a sketch or summary quantile, which differ slightly
// between sketch implementations and are compared within the percentile tolerance.
func isQuantile(path string) bool {
	i := strings.LastIndex(path, ".quantiles[")
	return i >= 0 && !strings.Contains(path[i+len(".quantiles["):], ".")
}

func (c *comparer) override(path string) (scenario.Override, bool) {
	for _, o := range c.overrides {
		if matchPattern(o.Path, path) {
			return o, true
		}
	}
	return scenario.Override{}, false
}

// overrideHolds applies a scenario comparator. Both sides must have a value: a field one side
// leaves out is a difference whatever the comparator.
func overrideHolds(o scenario.Override, reference, actual any) bool {
	if reference == nil || actual == nil {
		return false
	}
	switch o.Comparator {
	case "", "exact":
		return reflect.DeepEqual(reference, actual)
	case "equals":
		if a, okA := number(actual); okA {
			v, okV := number(o.Value)
			return okV && a == v
		}
		return reflect.DeepEqual(actual, o.Value)
	case "greater_than":
		a, okA := number(actual)
		v, okV := number(o.Value)
		return okA && okV && a > v
	case "within_percent":
		a, okA := number(actual)
		r, okR := number(reference)
		v, okV := number(o.Value)
		return okA && okR && okV && math.Abs(a-r) <= v/100*math.Abs(r)
	}
	return false
}

func number(v any) (float64, bool) {
	r := reflect.ValueOf(v)
	switch r.Kind() {
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		return float64(r.Int()), true
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64:
		return float64(r.Uint()), true
	case reflect.Float32, reflect.Float64:
		return r.Float(), true
	}
	return 0, false
}

// plain returns the value a report shows: nil for a missing or empty value, and the pointed-to
// value for a pointer.
func plain(v reflect.Value) any {
	if !v.IsValid() {
		return nil
	}
	switch v.Kind() {
	case reflect.Pointer, reflect.Interface:
		if v.IsNil() {
			return nil
		}
		return plain(v.Elem())
	case reflect.Slice, reflect.Map:
		if v.Len() == 0 {
			return nil
		}
	}
	return v.Interface()
}

func fieldName(f reflect.StructField) string {
	if !f.IsExported() {
		return ""
	}
	name, _, _ := strings.Cut(f.Tag.Get("json"), ",")
	switch name {
	case "-":
		return ""
	case "":
		return f.Name
	}
	return name
}

// matchPattern reports whether path matches pattern, in which * matches any run of characters
// other than "]".
func matchPattern(pattern, path string) bool {
	star := strings.IndexByte(pattern, '*')
	if star < 0 {
		return pattern == path
	}
	if !strings.HasPrefix(path, pattern[:star]) {
		return false
	}
	rest := path[star:]
	for i := 0; i <= len(rest); i++ {
		if matchPattern(pattern[star+1:], rest[i:]) {
			return true
		}
		if i < len(rest) && rest[i] == ']' {
			return false
		}
	}
	return false
}

// naturalLess orders paths with numbers in numeric order, so index=2 comes before index=10.
func naturalLess(a, b string) bool {
	for a != "" && b != "" {
		da, db := digitPrefix(a), digitPrefix(b)
		if da != "" && db != "" {
			if len(da) != len(db) {
				return len(da) < len(db)
			}
			if da != db {
				return da < db
			}
			a, b = a[len(da):], b[len(db):]
			continue
		}
		if a[0] != b[0] {
			return a[0] < b[0]
		}
		a, b = a[1:], b[1:]
	}
	return len(a) < len(b)
}

func digitPrefix(s string) string {
	i := 0
	for i < len(s) && s[i] >= '0' && s[i] <= '9' {
		i++
	}
	return s[:i]
}

func sortedKeys[V any](m map[string]V) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	slices.Sort(keys)
	return keys
}

func sortedTags(tags []string) string {
	sorted := slices.Clone(tags)
	slices.Sort(sorted)
	return strings.Join(sorted, "|")
}

// attributes adds, for every field of objects other than those in exclude, the sorted set of
// JSON values the field takes, under prefix plus the field name.
func attributes(into map[string][]string, prefix string, objects []any, exclude ...string) {
	var fields []map[string]json.RawMessage
	names := map[string]bool{}
	for _, o := range objects {
		var f map[string]json.RawMessage
		if err := json.Unmarshal([]byte(encode(o)), &f); err != nil {
			panic(fmt.Sprintf("decoding %T: %v", o, err))
		}
		for name := range f {
			if !slices.Contains(exclude, name) {
				names[name] = true
			}
		}
		fields = append(fields, f)
	}
	for name := range names {
		values := map[string]bool{}
		for _, f := range fields {
			value, ok := f[name]
			if ok {
				values[string(value)] = true
			} else {
				values[absent] = true
			}
		}
		into[prefix+name] = sortedKeys(values)
	}
}

func nilIfEmpty(m map[string][]string) map[string][]string {
	if len(m) == 0 {
		return nil
	}
	return m
}

func logItems(logs []normalize.Log) map[string]any {
	items := map[string]any{}
	counts := map[string]int{}
	for _, l := range logs {
		l.Tags = slices.Clone(l.Tags)
		slices.Sort(l.Tags)
		items[fmt.Sprintf("logs[ddsource=%s,index=%d]", l.Source, counts[l.Source])] = l
		counts[l.Source]++
	}
	return items
}

// encode returns the JSON form of v, without escaping characters such as < and >, which
// reports show to people.
func encode(v any) string {
	var out strings.Builder
	encoder := json.NewEncoder(&out)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		panic(fmt.Sprintf("encoding %T: %v", v, err))
	}
	return strings.TrimSuffix(out.String(), "\n")
}

// keyFields lists the non-empty fields of v as name=value, with the fields in first in that
// order and the others sorted by name.
func keyFields(prefix string, v any, first []string, exclude ...string) []string {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal([]byte(encode(v)), &fields); err != nil {
		panic(fmt.Sprintf("decoding %T: %v", v, err))
	}
	var rest []string
	for name := range fields {
		if !slices.Contains(exclude, name) && !slices.Contains(first, name) {
			rest = append(rest, name)
		}
	}
	slices.Sort(rest)
	var out []string
	for _, name := range append(slices.Clone(first), rest...) {
		raw, ok := fields[name]
		if !ok {
			continue
		}
		value := string(raw)
		var s string
		if json.Unmarshal(raw, &s) == nil {
			value = s
		}
		out = append(out, prefix+name+"="+value)
	}
	return out
}

// undecodedDifferences reports every undecoded payload. An entry on both sides is one
// difference with both values.
func undecodedDifferences(reference, actual []normalize.Undecoded) []diff.Difference {
	var out []diff.Difference
	path := func(u normalize.Undecoded) string {
		return fmt.Sprintf("undecoded[route=%s,error=%s]", u.Route, u.Error)
	}
	used := make([]bool, len(actual))
	for _, r := range reference {
		d := diff.Difference{Area: "undecoded", Path: path(r), Reference: r}
		for i, a := range actual {
			if !used[i] && a == r {
				used[i], d.Actual = true, a
				break
			}
		}
		out = append(out, d)
	}
	for i, a := range actual {
		if !used[i] {
			out = append(out, diff.Difference{Area: "undecoded", Path: path(a), Actual: a})
		}
	}
	return out
}
