package normalize

import (
	"encoding/json"
	"fmt"
	"slices"
	"strings"
)

// segment is one dotted part of a rule path: a field name, optionally followed by a selector in
// brackets. An empty selector, as in tags[], means every element of the list. A key, as in
// tags[env], selects the map entry with that key, or in a list of key:value strings, the value of
// every element with that key. A filter, as in series[metric=m] or series[metric!=m], selects the
// objects of a list whose field equals, or does not equal, the value; an object without the field
// does not equal it.
type segment struct {
	name     string
	selector bool
	key      string
	filter   *filter
}

// filter selects list elements by the value of one of their fields.
type filter struct {
	field, value string
	negate       bool
}

func (f filter) matches(element any) bool {
	object, _ := element.(map[string]any)
	value, ok := object[f.field]
	return (ok && fmt.Sprint(value) == f.value) != f.negate
}

// parseFilter reads a selector of the form field=value or field!=value, and returns nil for any
// other selector.
func parseFilter(selector string) (*filter, error) {
	field, value, ok := strings.Cut(selector, "=")
	if !ok {
		return nil, nil
	}
	f := &filter{field: field, value: value}
	if f.field, f.negate = strings.CutSuffix(field, "!"); f.field == "" || value == "" {
		return nil, fmt.Errorf("the filter [%s] needs a field and a value", selector)
	}
	return f, nil
}

// parsePath splits a rule path into segments. Dots inside brackets belong to the key.
func parsePath(path string) ([]segment, error) {
	if path == "" {
		return nil, fmt.Errorf("the path is empty")
	}
	var segments []segment
	for rest := path; ; {
		end := strings.IndexAny(rest, ".[")
		if end < 0 {
			end = len(rest)
		}
		s := segment{name: rest[:end]}
		if s.name == "" {
			return nil, fmt.Errorf("path %q has an empty field name", path)
		}
		rest = rest[end:]
		if strings.HasPrefix(rest, "[") {
			closing := strings.Index(rest, "]")
			if closing < 0 {
				return nil, fmt.Errorf("path %q has an unclosed bracket", path)
			}
			s.selector, s.key = true, rest[1:closing]
			filter, err := parseFilter(s.key)
			if err != nil {
				return nil, fmt.Errorf("path %q: %w", path, err)
			}
			if filter != nil {
				s.key, s.filter = "", filter
			}
			rest = rest[closing+1:]
		}
		segments = append(segments, s)
		if rest == "" {
			return segments, nil
		}
		if !strings.HasPrefix(rest, ".") {
			return nil, fmt.Errorf("path %q needs a dot after a bracket", path)
		}
		rest = rest[1:]
	}
}

// target is one value a path reaches.
type target struct {
	get    func() any
	set    func(any)
	remove func()
}

// walk calls visit for every value that segments reach in node, which is decoded JSON. Missing
// fields and values of the wrong kind are skipped, because not every payload has every field.
func walk(node any, segments []segment, visit func(target)) {
	object, ok := node.(map[string]any)
	if !ok {
		return
	}
	s, last := segments[0], len(segments) == 1
	child, ok := object[s.name]
	if !ok {
		return
	}
	switch {
	case !s.selector:
		if last {
			visit(target{
				get:    func() any { return object[s.name] },
				set:    func(v any) { object[s.name] = v },
				remove: func() { delete(object, s.name) },
			})
			return
		}
		walk(child, segments[1:], visit)
	case s.key == "":
		list, ok := child.([]any)
		if !ok {
			return
		}
		selected := func(i int) bool { return s.filter == nil || s.filter.matches(list[i]) }
		if !last {
			for i, element := range list {
				if selected(i) {
					walk(element, segments[1:], visit)
				}
			}
			return
		}
		walkList(object, s.name, list, func(i int, _ string) bool { return selected(i) }, visit)
	default:
		switch c := child.(type) {
		case map[string]any:
			value, ok := c[s.key]
			if !ok {
				return
			}
			if !last {
				walk(value, segments[1:], visit)
				return
			}
			visit(target{
				get:    func() any { return c[s.key] },
				set:    func(v any) { c[s.key] = v },
				remove: func() { delete(c, s.key) },
			})
		case []any:
			if !last {
				return
			}
			matches := func(_ int, tag string) bool {
				return strings.HasPrefix(tag, s.key+":")
			}
			walkList(object, s.name, c, matches, tagValue(s.key, visit))
		}
	}
}

// walkList visits the elements of object[name] that match, and removes the ones visit removes.
func walkList(object map[string]any, name string, list []any, match func(int, string) bool, visit func(target)) {
	removed := make([]bool, len(list))
	for i := range list {
		text, _ := list[i].(string)
		if !match(i, text) {
			continue
		}
		visit(target{
			get:    func() any { return list[i] },
			set:    func(v any) { list[i] = v },
			remove: func() { removed[i] = true },
		})
	}
	kept := list[:0]
	for i, element := range list {
		if !removed[i] {
			kept = append(kept, element)
		}
	}
	object[name] = kept
}

// tagValue turns targets on key:value tags into targets on their value.
func tagValue(key string, visit func(target)) func(target) {
	return func(t target) {
		visit(target{
			get: func() any {
				_, value, _ := strings.Cut(t.get().(string), ":")
				return value
			},
			set:    func(v any) { t.set(key + ":" + fmt.Sprint(v)) },
			remove: t.remove,
		})
	}
}

// labeler numbers values by order of first appearance, with one numbering per label.
type labeler map[string]map[string]string

func (l labeler) label(label string, value any) any {
	key := fmt.Sprint(value)
	if key == "" || key == "0" {
		return value
	}
	if l[label] == nil {
		l[label] = map[string]string{}
	}
	if _, ok := l[label][key]; !ok {
		l[label][key] = fmt.Sprintf("%s-%d", label, len(l[label])+1)
	}
	return l[label][key]
}

// sortList sorts strings by value and other elements by their JSON form.
func sortList(list []any) {
	sortKey := func(v any) string {
		if text, ok := v.(string); ok {
			return text
		}
		data, _ := json.Marshal(v)
		return string(data)
	}
	slices.SortStableFunc(list, func(a, b any) int { return strings.Compare(sortKey(a), sortKey(b)) })
}

// applyRule changes tree, the decoded JSON of a Normalized, as rule says. It reports whether the
// path reached any value.
func applyRule(tree map[string]any, rule Rule, segments []segment, labels labeler, window runWindow) (bool, error) {
	matcher, err := rule.valueMatcher()
	if err != nil {
		return false, fmt.Errorf("when_matches: %w", err)
	}
	matched := false
	walk(tree, segments, func(t target) {
		matched = true
		switch rule.Action {
		case "drop":
			t.remove()
		case "mask":
			if matcher != nil && !matcher.MatchString(fmt.Sprint(t.get())) {
				return
			}
			if maskErr := mask(t); maskErr != nil && err == nil {
				err = maskErr
			}
		case "relative":
			if relativeErr := relative(t, rule.Relative, window.start, window.exit); relativeErr != nil && err == nil {
				err = relativeErr
			}
		case "placeholder":
			t.set(placeholder)
		case "relabel":
			t.set(labels.label(rule.label(segments), t.get()))
		case "sort":
			if list, ok := t.get().([]any); ok {
				sortList(list)
			}
		}
	})
	return matched, err
}

// mask replaces a non-empty string with "<masked>" and a non-zero number with 1. Empty strings
// and zero numbers stay, so whether the value was sent is still compared. Numbers become 1, not
// 0, because the encoded payloads cannot tell a zero number from an absent one.
func mask(t target) error {
	switch value := t.get().(type) {
	case string:
		if value != "" {
			t.set(masked)
		}
	case json.Number:
		if f, err := value.Float64(); err != nil || f != 0 {
			t.set(json.Number("1"))
		}
	case nil:
	default:
		return fmt.Errorf("mask works on strings and numbers, not on %T", value)
	}
	return nil
}
