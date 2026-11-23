package workload

import "time"

// Clock is the time source of the driver. Tests replace it so that no real time passes.
type Clock interface {
	Now() time.Time
	Sleep(d time.Duration)
}

// RealClock uses the system clock.
type RealClock struct{}

// Now returns the current time.
func (RealClock) Now() time.Time { return time.Now() }

// Sleep pauses the calling goroutine for d.
func (RealClock) Sleep(d time.Duration) { time.Sleep(d) }
