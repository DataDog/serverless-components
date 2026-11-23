// Package diff holds the difference type that the content and timing comparisons share. It
// imports nothing from this module, so both comparisons can use it without importing each other.
package diff

// Difference is one way the actual capture differs from the reference capture.
type Difference struct {
	// Area is series, sketches, logs, traces, stats, process, undecoded or timing.
	Area string `json:"area"`
	// Path names the item and the field that differ, in the dotted form that the normalizer
	// rules use. internal/compare documents the item keys.
	Path string `json:"path"`
	// Reference and Actual are the two values. A side that has no such item or field is nil.
	Reference any `json:"reference"`
	Actual    any `json:"actual"`
	// Expected is true when expected-differences.yaml allows the difference, and Justification
	// is then the spec entry that justifies it.
	Expected      bool   `json:"expected,omitempty"`
	Justification string `json:"justification,omitempty"`
}
