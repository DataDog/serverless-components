// Package workload runs a scenario's workload steps at set times and logs what it did.
package workload

import (
	"fmt"
	"io"
	"os"
	"os/signal"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/events"
	"github.com/DataDog/serverless-components/parity/internal/scenario"
)

// childMarker starts every line that the driver prints.
const childMarker = "parity-child: "

// Hooks that tests replace.
var (
	arguments   = func() []string { return os.Args }
	environment = os.Environ
	// notifySignals starts delivering signals and returns the channel and a stop function.
	notifySignals = func() (<-chan os.Signal, func()) {
		ch := make(chan os.Signal, 8)
		signal.Notify(ch, syscall.SIGTERM, syscall.SIGINT, syscall.SIGHUP, syscall.SIGQUIT, syscall.SIGUSR1, syscall.SIGUSR2)
		return ch, func() { signal.Stop(ch) }
	}
	// raiseSignal restores the default handler of sig and sends sig to this process.
	raiseSignal = func(sig os.Signal) {
		signal.Reset(sig)
		if s, ok := sig.(syscall.Signal); ok {
			_ = syscall.Kill(os.Getpid(), s)
		}
	}
	// signalWaiting runs when a wait_for_signal step starts to wait.
	signalWaiting = func() {}
)

type runner struct {
	senders  Senders
	out      io.Writer
	errOut   io.Writer
	eventLog string
	clock    Clock
	signals  <-chan os.Signal

	mu     sync.Mutex
	waiter chan os.Signal // set while a wait_for_signal step waits
}

// Run executes steps, with each step's At measured from the start of the step clock, and
// returns the process exit code: the value of an exit step, or 0 when the steps end. The step
// clock starts at once, or at the next moment that align selects, and a clock_started event
// records when.
func Run(steps []scenario.Step, align scenario.Align, s Senders, out, errOut io.Writer, eventLog string, clock Clock) (exitCode int) {
	signals, stop := notifySignals()
	r := &runner{senders: s, out: out, errOut: errOut, eventLog: eventLog, clock: clock, signals: signals}
	done := make(chan struct{})
	var dispatcher sync.WaitGroup
	dispatcher.Add(1)
	go func() {
		defer dispatcher.Done()
		r.dispatchSignals(done)
	}()
	defer func() {
		stop()
		close(done)
		dispatcher.Wait()
	}()

	env := map[string]string{}
	for _, pair := range environment() {
		key, value, _ := strings.Cut(pair, "=")
		env[key] = value
	}
	r.log("started", map[string]any{"arguments": arguments(), "environment": env})

	// The event is written before the wait, with the time the step clock will start, so that the
	// harness can time the stop signal from it at once.
	start := align.Next(clock.Now())
	r.logAt("clock_started", map[string]any{}, start)
	if wait := start.Sub(clock.Now()); wait > 0 {
		clock.Sleep(wait)
	}
	code, _ := r.runSteps(steps, start)
	r.log("exiting", map[string]any{"code": code})
	return code
}

func (r *runner) log(kind string, detail map[string]any) {
	r.logAt(kind, detail, time.Time{})
}

// logAt writes an event stamped with at, or with the current time when at is the zero time.
func (r *runner) logAt(kind string, detail map[string]any, at time.Time) {
	if r.eventLog == "" {
		return
	}
	if at.IsZero() {
		at = r.clock.Now()
	}
	if err := events.Append(r.eventLog, events.Event{Time: at, Kind: kind, Detail: detail}); err != nil {
		fmt.Fprintf(r.errOut, "%scannot write event %q: %v\n", childMarker, kind, err)
	}
}

// dispatchSignals hands each trapped signal to the waiting wait_for_signal step.
// When no step waits, it logs the signal and re-raises it with the default
// handler, so the driver dies by the signal like a program that does not trap it.
func (r *runner) dispatchSignals(done <-chan struct{}) {
	for {
		select {
		case <-done:
			return
		case sig := <-r.signals:
			r.mu.Lock()
			waiter := r.waiter
			r.mu.Unlock()
			if waiter != nil {
				waiter <- sig
				continue
			}
			r.log("signal", map[string]any{"signal": signalName(sig)})
			raiseSignal(sig)
		}
	}
}

// waitForSignal blocks until the signal named name arrives, or any signal when name
// is empty, and logs every signal that arrives meanwhile. It returns the arrival time.
func (r *runner) waitForSignal(name string) time.Time {
	waiter := make(chan os.Signal, 8)
	r.mu.Lock()
	r.waiter = waiter
	r.mu.Unlock()
	defer func() {
		r.mu.Lock()
		r.waiter = nil
		r.mu.Unlock()
	}()
	signalWaiting()
	for {
		received := <-waiter
		r.log("signal", map[string]any{"signal": signalName(received)})
		if name == "" || name == signalName(received) {
			return r.clock.Now()
		}
	}
}

// runSteps runs steps with each At measured from start. It returns the exit code
// and whether an exit step ended the run.
func (r *runner) runSteps(steps []scenario.Step, start time.Time) (int, bool) {
	for _, step := range steps {
		if wait := step.At - r.clock.Now().Sub(start); wait > 0 {
			r.clock.Sleep(wait)
		}
		if code, exited := r.runStep(step); exited {
			return code, true
		}
	}
	return 0, false
}

func (r *runner) runStep(step scenario.Step) (int, bool) {
	switch step.Kind {
	case "send_dogstatsd":
		lines := stringList(step.Args["value"])
		sentAt, err := r.senders.DogStatsD(lines)
		r.logSent(step.Kind, map[string]any{"lines": lines}, sentAt, err)
	case "send_spans":
		fixture, _ := step.Args["fixture"].(string)
		sentAt, err := r.senders.Spans(fixture)
		r.logSent(step.Kind, map[string]any{"fixture": fixture}, sentAt, err)
	case "send_otlp":
		fixture, _ := step.Args["fixture"].(string)
		sentAt, err := r.senders.OTLP(fixture)
		r.logSent(step.Kind, map[string]any{"fixture": fixture}, sentAt, err)
	case "http_get":
		url, _ := step.Args["value"].(string)
		status, sentAt, err := r.senders.HTTPGet(url)
		r.logSent(step.Kind, map[string]any{"url": url, "status": status}, sentAt, err)
	case "print":
		r.print(step)
	case "append_log":
		r.appendLog(step)
	case "sleep":
		text, _ := step.Args["value"].(string)
		d, err := time.ParseDuration(text)
		if err != nil {
			fmt.Fprintf(r.errOut, "%sinvalid sleep %q: %v\n", childMarker, text, err)
			return 0, false
		}
		r.clock.Sleep(d)
	case "wait_for_signal":
		name, _ := step.Args["signal"].(string)
		return r.runSteps(step.Then, r.waitForSignal(name))
	case "exit":
		return exitValue(step.Args["value"]), true
	}
	return 0, false
}

// logSent logs a sent event stamped with the moment the item went on the wire.
func (r *runner) logSent(kind string, detail map[string]any, sentAt time.Time, err error) {
	detail["step"] = kind
	if err != nil {
		detail["error"] = err.Error()
		fmt.Fprintf(r.errOut, "%s%s failed: %v\n", childMarker, kind, err)
	}
	r.logAt("sent", detail, sentAt)
}

func (r *runner) print(step scenario.Step) {
	stream, _ := step.Args["stream"].(string)
	w := r.out
	if stream == "stderr" {
		w = r.errOut
	}
	for _, line := range stringList(step.Args["lines"]) {
		if !strings.HasPrefix(line, childMarker) {
			line = childMarker + line
		}
		fmt.Fprintln(w, line)
		r.log("printed", map[string]any{"stream": stream, "line": line})
	}
}

// appendLog appends the lines to the file named by the path argument.
func (r *runner) appendLog(step scenario.Step) {
	path, _ := step.Args["path"].(string)
	lines := stringList(step.Args["lines"])
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	sentAt := r.clock.Now()
	if err == nil {
		_, err = f.WriteString(strings.Join(lines, "\n") + "\n")
		if closeErr := f.Close(); err == nil {
			err = closeErr
		}
	}
	r.logSent("append_log", map[string]any{"path": path, "lines": lines}, sentAt, err)
}

func stringList(v any) []string {
	switch list := v.(type) {
	case []string:
		return list
	case []any:
		out := make([]string, len(list))
		for i, item := range list {
			out[i] = fmt.Sprint(item)
		}
		return out
	}
	return nil
}

func exitValue(v any) int {
	switch n := v.(type) {
	case int:
		return n
	case int64:
		return int(n)
	case float64:
		return int(n)
	}
	return 0
}

func signalName(s os.Signal) string {
	if sys, ok := s.(syscall.Signal); ok {
		switch sys {
		case syscall.SIGTERM:
			return "SIGTERM"
		case syscall.SIGINT:
			return "SIGINT"
		case syscall.SIGHUP:
			return "SIGHUP"
		case syscall.SIGQUIT:
			return "SIGQUIT"
		case syscall.SIGUSR1:
			return "SIGUSR1"
		case syscall.SIGUSR2:
			return "SIGUSR2"
		}
	}
	return s.String()
}
