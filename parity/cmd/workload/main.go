// Command parity-workload runs a scenario's workload steps inside a parity container.
package main

import (
	"flag"
	"fmt"
	"os"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/scenario"
	"github.com/DataDog/serverless-components/parity/internal/workload"
)

func main() {
	scenarioPath := flag.String("scenario", "/parity/scenario.yaml", "path to the scenario file")
	eventLog := flag.String("events", "/parity-shared/events.jsonl", "path to the event log")
	printClock := flag.Bool("print-clock", false, "print the current time in Unix nanoseconds and exit; the harness uses it to measure the Docker clock")
	flag.Parse()
	if *printClock {
		fmt.Println(time.Now().UnixNano())
		return
	}

	s, err := scenario.Load(*scenarioPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "parity-child: cannot load the scenario: %v\n", err)
		os.Exit(2)
	}
	senders := workload.NewRealSenders(*eventLog, os.Stderr)
	os.Exit(workload.Run(s.Workload, s.Align, senders, os.Stdout, os.Stderr, *eventLog, workload.RealClock{}))
}
