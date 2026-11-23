package workload

import (
	"bytes"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/events"
	"github.com/DataDog/serverless-components/parity/internal/scenario"
)

var epoch = time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)

type fakeClock struct {
	mu  sync.Mutex
	now time.Time
}

func (c *fakeClock) Now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.now
}

func (c *fakeClock) Sleep(d time.Duration) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.now = c.now.Add(d)
}

type fakeSenders struct {
	clock *fakeClock
	calls []string
	times []time.Time
	err   error
	// beforeWire and onWire advance the clock before and after the moment the item goes on the wire.
	beforeWire, afterWire time.Duration
	onSend                func()
}

// send returns the moment the item went on the wire.
func (f *fakeSenders) send(call string) time.Time {
	f.record(call)
	if f.onSend != nil {
		f.onSend()
	}
	f.clock.Sleep(f.beforeWire)
	at := f.clock.Now()
	f.clock.Sleep(f.afterWire)
	return at
}

func (f *fakeSenders) record(call string) {
	f.calls = append(f.calls, call)
	f.times = append(f.times, f.clock.Now())
}

func (f *fakeSenders) DogStatsD(lines []string) (time.Time, error) {
	return f.send("dogstatsd " + strings.Join(lines, ",")), f.err
}
func (f *fakeSenders) Spans(fixture string) (time.Time, error) {
	return f.send("spans " + fixture), f.err
}
func (f *fakeSenders) OTLP(fixture string) (time.Time, error) {
	return f.send("otlp " + fixture), f.err
}
func (f *fakeSenders) HTTPGet(url string) (int, time.Time, error) {
	return 200, f.send("get " + url), f.err
}

type harness struct {
	clock   *fakeClock
	senders *fakeSenders
	out     bytes.Buffer
	errOut  bytes.Buffer
	log     string
}

func newHarness(t *testing.T) *harness {
	clock := &fakeClock{now: epoch}
	return &harness{clock: clock, senders: &fakeSenders{clock: clock}, log: filepath.Join(t.TempDir(), "events.jsonl")}
}

func (h *harness) run(t *testing.T, steps []scenario.Step) int {
	return Run(steps, scenario.Align{}, h.senders, &h.out, &h.errOut, h.log, h.clock)
}

func (h *harness) events(t *testing.T) []events.Event {
	got, err := events.Read(h.log)
	if err != nil {
		t.Fatal(err)
	}
	return got
}

func kinds(es []events.Event) []string {
	var out []string
	for _, e := range es {
		out = append(out, e.Kind)
	}
	return out
}

func TestRunHonorsAtOffsets(t *testing.T) {
	h := newHarness(t)
	steps := []scenario.Step{
		{At: 0, Kind: "send_dogstatsd", Args: map[string]any{"value": []any{"a:1|c"}}},
		{At: 5 * time.Second, Kind: "send_spans", Args: map[string]any{"fixture": "two-traces-v04"}},
	}
	if code := h.run(t, steps); code != 0 {
		t.Fatalf("exit code %d, want 0", code)
	}
	wantCalls := []string{"dogstatsd a:1|c", "spans two-traces-v04"}
	wantTimes := []time.Time{epoch, epoch.Add(5 * time.Second)}
	if !reflect.DeepEqual(h.senders.calls, wantCalls) || !reflect.DeepEqual(h.senders.times, wantTimes) {
		t.Fatalf("calls %v at %v, want %v at %v", h.senders.calls, h.senders.times, wantCalls, wantTimes)
	}
	if got, want := kinds(h.events(t)), []string{"started", "clock_started", "sent", "sent", "exiting"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("event kinds %v, want %v", got, want)
	}
}

func TestRunPrintsWithChildMarker(t *testing.T) {
	h := newHarness(t)
	steps := []scenario.Step{
		{Kind: "print", Args: map[string]any{"stream": "stdout", "lines": []any{"started", "parity-child: already marked"}}},
		{Kind: "print", Args: map[string]any{"stream": "stderr", "lines": []any{"oops"}}},
	}
	h.run(t, steps)
	if got, want := h.out.String(), "parity-child: started\nparity-child: already marked\n"; got != want {
		t.Fatalf("stdout %q, want %q", got, want)
	}
	if got, want := h.errOut.String(), "parity-child: oops\n"; got != want {
		t.Fatalf("stderr %q, want %q", got, want)
	}
	if got, want := kinds(h.events(t)), []string{"started", "clock_started", "printed", "printed", "printed", "exiting"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("event kinds %v, want %v", got, want)
	}
}

func TestRunExitCode(t *testing.T) {
	h := newHarness(t)
	steps := []scenario.Step{
		{Kind: "exit", Args: map[string]any{"value": 7}},
		{Kind: "print", Args: map[string]any{"stream": "stdout", "lines": []any{"never"}}},
	}
	if code := h.run(t, steps); code != 7 {
		t.Fatalf("exit code %d, want 7", code)
	}
	got := h.events(t)
	last := got[len(got)-1]
	if last.Kind != "exiting" || last.Detail["code"] != float64(7) {
		t.Fatalf("last event %+v, want exiting with code 7", last)
	}
	if h.out.Len() != 0 {
		t.Fatalf("stdout %q, want empty", h.out.String())
	}
}

// injectSignals replaces the signal hooks: the signals are delivered when a
// wait_for_signal step starts to wait, and re-raised signals are recorded.
func injectSignals(t *testing.T, delivered chan os.Signal, onWaiting func()) *[]os.Signal {
	var mu sync.Mutex
	raised := &[]os.Signal{}
	originalNotify, originalRaise, originalWaiting := notifySignals, raiseSignal, signalWaiting
	notifySignals = func() (<-chan os.Signal, func()) { return delivered, func() {} }
	raiseSignal = func(sig os.Signal) { mu.Lock(); *raised = append(*raised, sig); mu.Unlock() }
	signalWaiting = onWaiting
	t.Cleanup(func() { notifySignals, raiseSignal, signalWaiting = originalNotify, originalRaise, originalWaiting })
	return raised
}

func TestRunWaitForSignalRunsThenSteps(t *testing.T) {
	tests := []struct {
		name      string
		delivered []os.Signal
		wantKinds []string
		wantLogs  []any
	}{
		{"matching signal", []os.Signal{syscall.SIGTERM}, []string{"started", "clock_started", "signal", "sent", "printed", "exiting"}, []any{"SIGTERM"}},
		{"non-matching signal first", []os.Signal{syscall.SIGINT, syscall.SIGTERM}, []string{"started", "clock_started", "signal", "signal", "sent", "printed", "exiting"}, []any{"SIGINT", "SIGTERM"}},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			h := newHarness(t)
			delivered := make(chan os.Signal, 8)
			raised := injectSignals(t, delivered, func() {
				for _, sig := range tt.delivered {
					delivered <- sig
				}
			})
			steps := []scenario.Step{
				{Kind: "sleep", Args: map[string]any{"value": "3s"}},
				{Kind: "wait_for_signal", Args: map[string]any{"signal": "SIGTERM"}, Then: []scenario.Step{
					{At: 2 * time.Second, Kind: "send_dogstatsd", Args: map[string]any{"value": []any{"a:1|c"}}},
					{Kind: "print", Args: map[string]any{"stream": "stdout", "lines": []any{"got SIGTERM"}}},
					{Kind: "exit", Args: map[string]any{"value": 7}},
				}},
			}
			if code := h.run(t, steps); code != 7 {
				t.Fatalf("exit code %d, want 7", code)
			}
			if got, want := h.out.String(), "parity-child: got SIGTERM\n"; got != want {
				t.Fatalf("stdout %q, want %q", got, want)
			}
			got := h.events(t)
			if !reflect.DeepEqual(kinds(got), tt.wantKinds) {
				t.Fatalf("event kinds %v, want %v", kinds(got), tt.wantKinds)
			}
			var logged []any
			for _, e := range got {
				if e.Kind == "signal" {
					logged = append(logged, e.Detail["signal"])
				}
			}
			if !reflect.DeepEqual(logged, tt.wantLogs) {
				t.Fatalf("logged signals %v, want %v", logged, tt.wantLogs)
			}
			if want := epoch.Add(5 * time.Second); !h.senders.times[0].Equal(want) {
				t.Fatalf("then step ran at %v, want %v (2s after the signal at 3s)", h.senders.times[0], want)
			}
			if len(*raised) != 0 {
				t.Fatalf("re-raised %v while waiting, want none", *raised)
			}
		})
	}
}

func TestRunReRaisesSignalWhenNotWaiting(t *testing.T) {
	h := newHarness(t)
	delivered := make(chan os.Signal, 1)
	raised := injectSignals(t, delivered, func() {})
	reRaised := make(chan struct{})
	original := raiseSignal
	raiseSignal = func(sig os.Signal) { original(sig); close(reRaised) }
	h.senders.onSend = func() {
		delivered <- syscall.SIGTERM
		<-reRaised
	}
	h.run(t, []scenario.Step{{Kind: "send_dogstatsd", Args: map[string]any{"value": []any{"a:1|c"}}}})
	if !reflect.DeepEqual(*raised, []os.Signal{syscall.SIGTERM}) {
		t.Fatalf("re-raised %v, want [SIGTERM]", *raised)
	}
	if got, want := kinds(h.events(t)), []string{"started", "clock_started", "signal", "sent", "exiting"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("event kinds %v, want %v", got, want)
	}
}

func TestRunSentTimeIsWhenItWentOnTheWire(t *testing.T) {
	h := newHarness(t)
	h.senders.beforeWire = 10 * time.Second // a wait for the receiver
	h.senders.afterWire = time.Second       // the transfer itself
	h.run(t, []scenario.Step{{Kind: "send_spans", Args: map[string]any{"fixture": "two-traces-v04"}}})
	sent := h.events(t)[2]
	if want := epoch.Add(10 * time.Second); sent.Kind != "sent" || !sent.Time.Equal(want) {
		t.Fatalf("event %s at %v, want sent at %v", sent.Kind, sent.Time, want)
	}
}

func TestRunLogsStartedWithArgumentsAndEnvironment(t *testing.T) {
	h := newHarness(t)
	originalArguments, originalEnvironment := arguments, environment
	arguments = func() []string { return []string{"parity-workload", "--scenario", "s.yaml"} }
	environment = func() []string { return []string{"DD_SERVICE=parity-app", "EMPTY="} }
	t.Cleanup(func() { arguments, environment = originalArguments, originalEnvironment })

	h.run(t, nil)
	started := h.events(t)[0]
	if started.Kind != "started" {
		t.Fatalf("first event %q, want started", started.Kind)
	}
	if got, want := started.Detail["arguments"], []any{"parity-workload", "--scenario", "s.yaml"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("arguments %v, want %v", got, want)
	}
	if got, want := started.Detail["environment"], map[string]any{"DD_SERVICE": "parity-app", "EMPTY": ""}; !reflect.DeepEqual(got, want) {
		t.Fatalf("environment %v, want %v", got, want)
	}
}

func TestRunRecordsSenderErrorAndContinues(t *testing.T) {
	h := newHarness(t)
	h.senders.err = errors.New("connection refused")
	steps := []scenario.Step{
		{Kind: "send_otlp", Args: map[string]any{"fixture": "x"}},
		{Kind: "sleep", Args: map[string]any{"value": "2s"}},
		{Kind: "http_get", Args: map[string]any{"value": "http://127.0.0.1:8126/info"}},
	}
	h.run(t, steps)
	got := h.events(t)
	if want := []string{"started", "clock_started", "sent", "sent", "exiting"}; !reflect.DeepEqual(kinds(got), want) {
		t.Fatalf("event kinds %v, want %v", kinds(got), want)
	}
	if got[2].Detail["error"] != "connection refused" || got[3].Detail["error"] != "connection refused" {
		t.Fatalf("details %v %v, want error recorded", got[2].Detail, got[3].Detail)
	}
	if want := epoch.Add(2 * time.Second); !h.senders.times[1].Equal(want) {
		t.Fatalf("http_get ran at %v, want %v", h.senders.times[1], want)
	}
}

// With align, the steps are timed from the next aligned moment, and the clock_started event
// records it.
func TestRunAlignsTheStepClock(t *testing.T) {
	h := newHarness(t)
	h.clock.now = epoch.Add(3200 * time.Millisecond)
	steps := []scenario.Step{{At: 2 * time.Second, Kind: "send_dogstatsd", Args: map[string]any{"value": []any{"a:1|c"}}}}
	Run(steps, scenario.Align{Every: 10 * time.Second, Offset: 500 * time.Millisecond}, h.senders, &h.out, &h.errOut, h.log, h.clock)
	if want := []time.Time{epoch.Add(12500 * time.Millisecond)}; !reflect.DeepEqual(h.senders.times, want) {
		t.Fatalf("sent at %v, want %v", h.senders.times, want)
	}
	if es := h.events(t); es[1].Kind != "clock_started" || !es[1].Time.Equal(epoch.Add(10500*time.Millisecond)) {
		t.Fatalf("second event %+v, want clock_started at %v", es[1], epoch.Add(10500*time.Millisecond))
	}
}
