package normalize

import (
	"encoding/json"
	"fmt"
	"strconv"
	"time"
)

// runWindow is when the agent container ran, on the Docker clock.
type runWindow struct{ start, exit time.Time }

// windowMargin widens the run window on both sides, for the clock granularity of the container.
const windowMargin = time.Second

// units are the time units a relative rule can declare.
var units = map[string]time.Duration{"s": time.Second, "ms": time.Millisecond, "ns": time.Nanosecond}

// Relative configures a relative rule. Unit is the unit of the timestamp: s, ms or ns. Lead is how
// long before the container start a timestamp may be, for the start of a time bucket that began
// before the agent. Align, when set, is the bucket length that every timestamp must be a multiple
// of since the Unix epoch.
type Relative struct {
	Unit  string        `yaml:"unit"`
	Align time.Duration `yaml:"align,omitempty"`
	Lead  time.Duration `yaml:"lead,omitempty"`
}

func (r Relative) validate() error {
	unit, ok := units[r.Unit]
	if !ok {
		return fmt.Errorf("relative.unit is %q, want s, ms or ns", r.Unit)
	}
	if r.Lead < 0 {
		return fmt.Errorf("relative.lead is %s; it must not be negative", r.Lead)
	}
	if r.Align < 0 || r.Align%unit != 0 {
		return fmt.Errorf("relative.align is %s; it must be a positive multiple of the unit %s", r.Align, r.Unit)
	}
	return nil
}

// relativeLabel returns the label of a timestamp in the unit r declares, against the run window
// from start to exit. The window is widened by windowMargin on both sides, and by r.Lead before
// the start. A timestamp in the window is "in window", with ", aligned" when r declares an
// alignment that it holds. Otherwise the label keeps the value, so that the report shows it.
func relativeLabel(value int64, r Relative, start, exit time.Time) string {
	if start.IsZero() || exit.IsZero() {
		return fmt.Sprintf("no run window: %d", value)
	}
	unit := int64(units[r.Unit])
	// The bounds are converted to the unit, not the value to nanoseconds, which could overflow.
	low := floorDiv(start.Add(-windowMargin-r.Lead).UnixNano(), unit)
	high := floorDiv(exit.Add(windowMargin).UnixNano(), unit)
	if value < low || value > high {
		return fmt.Sprintf("out of window: %d", value)
	}
	if r.Align == 0 {
		return "in window"
	}
	if value%(int64(r.Align)/unit) != 0 {
		return fmt.Sprintf("not aligned: %d", value)
	}
	return "in window, aligned"
}

func floorDiv(a, b int64) int64 {
	q := a / b
	if a%b != 0 && (a < 0) != (b < 0) {
		q--
	}
	return q
}

// relative replaces a non-zero number with its relative label. Zero stays, so whether the value
// was sent is still compared.
func relative(t target, r Relative, start, exit time.Time) error {
	switch value := t.get().(type) {
	case json.Number:
		n, err := value.Int64()
		if err != nil {
			return fmt.Errorf("relative works on whole numbers, not on %s", value)
		}
		if n != 0 {
			t.set(relativeLabel(n, r, start, exit))
		}
	case nil:
	default:
		return fmt.Errorf("relative works on numbers, not on %T", value)
	}
	return nil
}

// Timestamp is a time value of a payload: a number, or the label that a relative rule gave it.
type Timestamp struct {
	Value int64
	Label string
}

// IsZero reports whether the timestamp was not sent, so that omitzero leaves it out.
func (t Timestamp) IsZero() bool { return t == Timestamp{} }

func (t Timestamp) MarshalJSON() ([]byte, error) {
	if t.Label != "" {
		return json.Marshal(t.Label)
	}
	return []byte(strconv.FormatInt(t.Value, 10)), nil
}

func (t *Timestamp) UnmarshalJSON(data []byte) error {
	if len(data) > 0 && data[0] == '"' {
		*t = Timestamp{}
		return json.Unmarshal(data, &t.Label)
	}
	*t = Timestamp{}
	return json.Unmarshal(data, &t.Value)
}
